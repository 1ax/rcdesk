//! File transfer over the `file` data channel (slice 3.4, see
//! `proto::file` for the wire protocol). This sub-step (3.4a) implements the
//! client -> host direction: the host receives a file into the user's
//! Downloads folder.
//!
//! The channel is served by one task per session (`run`), fed by the
//! transport's dedicated `file` mpsc (`transport::FileChannelMessage`)
//! instead of `SessionEvent::DataChannelMessage`, so disk writes never run
//! inside `signaling::handle_session_event`. Replies go out through the
//! `FileChannel` trait so the logic is testable without WebRTC; the host ->
//! client direction (3.4c) will reuse the same task and trait.
//!
//! Disk safety: data is written to `<name>.rcdesk-part`; the final name is
//! reserved up front with `create_new` (an empty placeholder, so two
//! transfers can never pick the same name) and the part file replaces it on
//! a successful `End`. Anything else (abort, protocol error, channel loss,
//! task cancellation) removes both files.
//!
//! Privacy: file names are logged at `debug` only, never file content.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use proto::file::{AbortReason, FileMessage};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannel;

use crate::transport::FileChannelMessage;

#[cfg(target_os = "windows")]
pub mod windows;

/// Extension of the in-progress file next to the final one.
const PART_SUFFIX: &str = ".rcdesk-part";

/// Longest sanitized file name, in UTF-8 bytes.
const MAX_NAME_BYTES: usize = 200;

/// An extension longer than this (including the dot) is not preserved when
/// a long name is truncated.
const MAX_KEPT_EXT_BYTES: usize = 20;

/// Where incoming files land: the user's Downloads folder.
pub fn receive_dir() -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        windows::downloads_dir()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("HOME is not set, cannot locate Downloads"))?;
        Ok(PathBuf::from(home).join("Downloads"))
    }
}

/// The sending half of the `file` channel as the transfer logic sees it.
#[async_trait::async_trait]
pub trait FileChannel: Send + Sync {
    async fn send_text(&self, text: &str) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl FileChannel for Arc<dyn DataChannel> {
    async fn send_text(&self, text: &str) -> anyhow::Result<()> {
        DataChannel::send_text(self.as_ref(), text).await?;
        Ok(())
    }
}

fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end();
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let b = upper.as_bytes();
            b.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && (b'1'..=b'9').contains(&b[3])
        }
    }
}

fn trim_tail(s: &str) -> &str {
    s.trim_end_matches(['.', ' '])
}

/// Turns a file name received from the peer into one that is safe to create
/// on any supported OS (same on all of them, so behavior does not depend on
/// the host platform). See the 3.4a spec for the rules.
pub fn sanitize_file_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let replaced: String = last
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut name = trim_tail(&replaced).to_string();
    if name.len() > MAX_NAME_BYTES {
        let ext_start = name.rfind('.').filter(|&i| i > 0);
        let ext = ext_start
            .map(|i| &name[i..])
            .filter(|e| e.len() <= MAX_KEPT_EXT_BYTES)
            .unwrap_or("");
        let stem_src = &name[..name.len() - ext.len()];
        let mut cut = MAX_NAME_BYTES - ext.len();
        while !stem_src.is_char_boundary(cut) {
            cut -= 1;
        }
        let stem = trim_tail(&stem_src[..cut]);
        name = format!("{stem}{ext}");
    }
    if name.is_empty() || name == "." || name == ".." {
        name = "file".to_string();
    }
    if is_windows_reserved(&name) {
        name.insert(0, '_');
    }
    name
}

/// `name.ext` -> `name (n).ext` (`n == 0` keeps the name as is).
fn with_suffix(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_string();
    }
    match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => format!("{} ({n}){}", &name[..i], &name[i..]),
        None => format!("{name} ({n})"),
    }
}

/// Reserves a free final name in `dir` (`create_new`, race-free) together
/// with its `.rcdesk-part` file. Returns the part file, its path, the final
/// path and the final file name.
async fn create_unique(
    dir: &Path,
    name: &str,
) -> std::io::Result<(tokio::fs::File, PathBuf, PathBuf, String)> {
    for n in 0..10_000u32 {
        let candidate = with_suffix(name, n);
        let final_path = dir.join(&candidate);
        let part_path = dir.join(format!("{candidate}{PART_SUFFIX}"));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&final_path)
            .await
        {
            Ok(_placeholder) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
            .await
        {
            Ok(file) => return Ok((file, part_path, final_path, candidate)),
            Err(e) => {
                let _ = tokio::fs::remove_file(&final_path).await;
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free file name",
    ))
}

/// One transfer in progress. Dropping it without `finished` removes the
/// part file and the reserved placeholder (also on task cancellation).
struct ActiveReceive {
    id: u32,
    size: u64,
    written: u64,
    file: Option<tokio::fs::File>,
    part_path: PathBuf,
    final_path: PathBuf,
    final_name: String,
    finished: bool,
}

impl Drop for ActiveReceive {
    fn drop(&mut self) {
        if !self.finished {
            // Close the handle first (an open file cannot be removed on
            // Windows).
            self.file.take();
            // Best effort: nothing useful to do if removal fails.
            let _ = std::fs::remove_file(&self.part_path);
            let _ = std::fs::remove_file(&self.final_path);
        }
    }
}

/// Receiving side of the `file` channel for one session.
struct Receiver {
    channel: Arc<dyn FileChannel>,
    /// `None`: file transfer is disabled (`--no-files`, or no download
    /// folder could be determined).
    dir: Option<PathBuf>,
    active: Option<ActiveReceive>,
}

impl Receiver {
    async fn send(&self, msg: &FileMessage) {
        match serde_json::to_string(msg) {
            Ok(json) => {
                if let Err(err) = self.channel.send_text(&json).await {
                    tracing::debug!(?err, "failed to send file channel message");
                }
            }
            Err(err) => tracing::warn!(?err, "failed to encode file channel message"),
        }
    }

    async fn abort(&mut self, id: u32, reason: AbortReason) {
        self.active = None;
        self.send(&FileMessage::Abort { id, reason }).await;
    }

    async fn on_message(&mut self, msg: FileChannelMessage) {
        if msg.is_string {
            match serde_json::from_slice::<FileMessage>(&msg.data) {
                Ok(m) => self.on_control(m).await,
                Err(err) => tracing::warn!(?err, "invalid file channel message, ignoring"),
            }
        } else {
            self.on_chunk(&msg.data).await;
        }
    }

    async fn on_control(&mut self, msg: FileMessage) {
        match msg {
            FileMessage::Start { id, name, size } => self.on_start(id, name, size).await,
            FileMessage::End { id } => self.on_end(id).await,
            FileMessage::Abort { id, .. } => {
                if self.active.as_ref().is_some_and(|a| a.id == id) {
                    tracing::debug!(id, "file receive aborted by peer");
                    self.active = None;
                }
            }
            FileMessage::Saved { .. } => {
                tracing::trace!("unexpected saved message from client, ignoring");
            }
        }
    }

    async fn on_start(&mut self, id: u32, name: String, size: u64) {
        if let Some(old) = self.active.take() {
            tracing::debug!(id = old.id, "new start over active receive, aborting old");
            let old_id = old.id;
            drop(old);
            self.send(&FileMessage::Abort {
                id: old_id,
                reason: AbortReason::Protocol,
            })
            .await;
        }
        let Some(dir) = self.dir.clone() else {
            self.send(&FileMessage::Abort {
                id,
                reason: AbortReason::Disabled,
            })
            .await;
            return;
        };
        let clean = sanitize_file_name(&name);
        let created = match tokio::fs::create_dir_all(&dir).await {
            Ok(()) => create_unique(&dir, &clean).await,
            Err(e) => Err(e),
        };
        match created {
            Ok((file, part_path, final_path, final_name)) => {
                tracing::debug!(id, name = %final_name, size, "file receive started");
                self.active = Some(ActiveReceive {
                    id,
                    size,
                    written: 0,
                    file: Some(file),
                    part_path,
                    final_path,
                    final_name,
                    finished: false,
                });
            }
            Err(err) => {
                tracing::warn!(?err, "cannot create file for incoming transfer");
                self.send(&FileMessage::Abort {
                    id,
                    reason: AbortReason::Io,
                })
                .await;
            }
        }
    }

    async fn on_chunk(&mut self, data: &[u8]) {
        let Some(active) = self.active.as_mut() else {
            tracing::trace!(
                len = data.len(),
                "file chunk without active receive, dropping"
            );
            return;
        };
        let id = active.id;
        if active.written + data.len() as u64 > active.size {
            tracing::warn!(id, "file chunk exceeds announced size");
            self.abort(id, AbortReason::Protocol).await;
            return;
        }
        let result = match active.file.as_mut() {
            Some(file) => file.write_all(data).await,
            None => Ok(()),
        };
        match result {
            Ok(()) => active.written += data.len() as u64,
            Err(err) => {
                tracing::warn!(?err, id, "file write failed");
                self.abort(id, AbortReason::Io).await;
            }
        }
    }

    async fn on_end(&mut self, id: u32) {
        let Some(active) = self.active.as_mut().filter(|a| a.id == id) else {
            tracing::trace!(id, "end without matching active receive, ignoring");
            return;
        };
        if active.written != active.size {
            tracing::warn!(
                id,
                written = active.written,
                size = active.size,
                "file ended with wrong size"
            );
            self.abort(id, AbortReason::Protocol).await;
            return;
        }
        let flushed = match active.file.take() {
            Some(mut file) => match file.flush().await {
                // Close the handle before renaming (required on Windows).
                Ok(()) => {
                    drop(file);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            None => Ok(()),
        };
        let renamed = match flushed {
            Ok(()) => tokio::fs::rename(&active.part_path, &active.final_path).await,
            Err(e) => Err(e),
        };
        match renamed {
            Ok(()) => {
                active.finished = true;
                let name = active.final_name.clone();
                let size = active.size;
                self.active = None;
                tracing::info!(size, name = %name, "file received");
                self.send(&FileMessage::Saved { id, name }).await;
            }
            Err(err) => {
                tracing::warn!(?err, id, "finishing received file failed");
                self.abort(id, AbortReason::Io).await;
            }
        }
    }
}

/// Serves the `file` channel for one session until `rx` closes (the channel
/// or the session ended) or the task is aborted; either way an unfinished
/// transfer is cleaned up. `dir` is the receive folder, or `None` when file
/// transfer is disabled -- every `Start` is then answered with
/// `Abort { Disabled }` and nothing is created.
pub async fn run(
    channel: Arc<dyn FileChannel>,
    mut rx: mpsc::Receiver<FileChannelMessage>,
    dir: Option<PathBuf>,
) {
    let mut receiver = Receiver {
        channel,
        dir,
        active: None,
    };
    while let Some(msg) = rx.recv().await {
        receiver.on_message(msg).await;
    }
    if receiver.active.take().is_some() {
        tracing::debug!("file channel ended mid-transfer, partial file removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    #[test]
    fn sanitize_table() {
        let long_ext = format!("{}.txt", "я".repeat(150));
        let cases: Vec<(String, String)> = vec![
            ("a.txt".into(), "a.txt".into()),
            ("dir/sub/a.txt".into(), "a.txt".into()),
            ("C:\\Users\\x\\a.txt".into(), "a.txt".into()),
            ("a/b\\c.txt".into(), "c.txt".into()),
            ("a\u{1}b\tc.txt".into(), "a_b_c.txt".into()),
            ("a<b>c:d\"e|f?g*h.txt".into(), "a_b_c_d_e_f_g_h.txt".into()),
            ("name. . ".into(), "name".into()),
            ("a.txt...".into(), "a.txt".into()),
            (".".into(), "file".into()),
            ("..".into(), "file".into()),
            ("".into(), "file".into()),
            ("dir/".into(), "file".into()),
            ("   ".into(), "file".into()),
            ("CON".into(), "_CON".into()),
            ("con.txt".into(), "_con.txt".into()),
            ("Nul.tar.gz".into(), "_Nul.tar.gz".into()),
            ("prn".into(), "_prn".into()),
            ("AUX.x".into(), "_AUX.x".into()),
            ("COM1".into(), "_COM1".into()),
            ("com9.log".into(), "_com9.log".into()),
            ("LPT5.txt".into(), "_LPT5.txt".into()),
            ("COM0".into(), "COM0".into()),
            ("COM10".into(), "COM10".into()),
            ("console.txt".into(), "console.txt".into()),
            (".bashrc".into(), ".bashrc".into()),
            ("отчёт.pdf".into(), "отчёт.pdf".into()),
        ];
        for (raw, expected) in cases {
            assert_eq!(sanitize_file_name(&raw), expected, "raw = {raw:?}");
        }
        // Over-long names: <= 200 bytes, valid UTF-8, extension kept.
        let out = sanitize_file_name(&long_ext);
        assert!(out.len() <= MAX_NAME_BYTES, "len {}", out.len());
        assert!(out.ends_with(".txt"));
        assert!(out.starts_with('я'));
        let no_ext = sanitize_file_name(&"ж".repeat(150));
        assert!(no_ext.len() <= MAX_NAME_BYTES);
        assert!(no_ext.chars().all(|c| c == 'ж'));
        // An absurdly long "extension" is not preserved but the cap holds.
        let weird = format!("a.{}", "b".repeat(300));
        assert!(sanitize_file_name(&weird).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn suffix_goes_before_extension() {
        assert_eq!(with_suffix("a.txt", 0), "a.txt");
        assert_eq!(with_suffix("a.txt", 1), "a (1).txt");
        assert_eq!(with_suffix("a.tar.gz", 2), "a.tar (2).gz");
        assert_eq!(with_suffix("noext", 3), "noext (3)");
        assert_eq!(with_suffix(".bashrc", 1), ".bashrc (1)");
    }

    struct FakeChannel {
        sent: Mutex<Vec<FileMessage>>,
    }

    #[async_trait::async_trait]
    impl FileChannel for FakeChannel {
        async fn send_text(&self, text: &str) -> anyhow::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push(serde_json::from_str(text).unwrap());
            Ok(())
        }
    }

    fn scratch_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-files-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn text(msg: &FileMessage) -> FileChannelMessage {
        FileChannelMessage {
            data: Bytes::from(serde_json::to_vec(msg).unwrap()),
            is_string: true,
        }
    }

    fn chunk(data: &[u8]) -> FileChannelMessage {
        FileChannelMessage {
            data: Bytes::copy_from_slice(data),
            is_string: false,
        }
    }

    fn start(id: u32, name: &str, size: u64) -> FileChannelMessage {
        text(&FileMessage::Start {
            id,
            name: name.to_string(),
            size,
        })
    }

    struct Harness {
        receiver: Receiver,
        fake: Arc<FakeChannel>,
        dir: PathBuf,
    }

    impl Harness {
        fn new(enabled: bool) -> Self {
            let dir = scratch_dir();
            let fake = Arc::new(FakeChannel {
                sent: Mutex::new(vec![]),
            });
            let receiver = Receiver {
                channel: Arc::clone(&fake) as Arc<dyn FileChannel>,
                dir: enabled.then(|| dir.clone()),
                active: None,
            };
            Self {
                receiver,
                fake,
                dir,
            }
        }

        async fn feed(&mut self, msgs: Vec<FileChannelMessage>) {
            for m in msgs {
                self.receiver.on_message(m).await;
            }
        }

        fn sent(&self) -> Vec<FileMessage> {
            self.fake.sent.lock().unwrap().clone()
        }

        fn files(&self) -> Vec<String> {
            let mut v: Vec<String> = match std::fs::read_dir(&self.dir) {
                Ok(rd) => rd
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect(),
                Err(_) => vec![],
            };
            v.sort();
            v
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn receives_a_file_and_replies_saved() {
        let mut h = Harness::new(true);
        h.feed(vec![
            start(1, "dir/hello.txt", 11),
            chunk(b"hello "),
            chunk(b"world"),
            text(&FileMessage::End { id: 1 }),
        ])
        .await;
        assert_eq!(
            h.sent(),
            vec![FileMessage::Saved {
                id: 1,
                name: "hello.txt".into()
            }]
        );
        assert_eq!(h.files(), vec!["hello.txt".to_string()]);
        assert_eq!(
            std::fs::read(h.dir.join("hello.txt")).unwrap(),
            b"hello world"
        );
    }

    #[tokio::test]
    async fn colliding_names_get_numeric_suffixes() {
        let mut h = Harness::new(true);
        for id in 1..=3 {
            h.feed(vec![
                start(id, "a.txt", 1),
                chunk(b"x"),
                text(&FileMessage::End { id }),
            ])
            .await;
        }
        assert_eq!(
            h.files(),
            vec![
                "a (1).txt".to_string(),
                "a (2).txt".to_string(),
                "a.txt".to_string()
            ]
        );
        let names: Vec<String> = h
            .sent()
            .into_iter()
            .map(|m| match m {
                FileMessage::Saved { name, .. } => name,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["a.txt", "a (1).txt", "a (2).txt"]);
    }

    #[tokio::test]
    async fn existing_part_file_is_not_reused() {
        let h0 = Harness::new(true);
        std::fs::create_dir_all(&h0.dir).unwrap();
        std::fs::write(h0.dir.join("a.txt.rcdesk-part"), b"stale").unwrap();
        let mut h = h0;
        h.feed(vec![
            start(1, "a.txt", 0),
            text(&FileMessage::End { id: 1 }),
        ])
        .await;
        assert_eq!(
            h.sent(),
            vec![FileMessage::Saved {
                id: 1,
                name: "a (1).txt".into()
            }]
        );
        assert_eq!(
            std::fs::read(h.dir.join("a.txt.rcdesk-part")).unwrap(),
            b"stale"
        );
    }

    #[tokio::test]
    async fn oversized_data_aborts_with_protocol_and_cleans_up() {
        let mut h = Harness::new(true);
        h.feed(vec![start(1, "a.bin", 4), chunk(b"abc"), chunk(b"de")])
            .await;
        assert_eq!(
            h.sent(),
            vec![FileMessage::Abort {
                id: 1,
                reason: AbortReason::Protocol
            }]
        );
        assert!(h.files().is_empty());
        // Later chunks and End for the dead transfer are ignored.
        h.feed(vec![chunk(b"f"), text(&FileMessage::End { id: 1 })])
            .await;
        assert_eq!(h.sent().len(), 1);
    }

    #[tokio::test]
    async fn short_data_at_end_aborts_with_protocol() {
        let mut h = Harness::new(true);
        h.feed(vec![
            start(1, "a.bin", 10),
            chunk(b"abc"),
            text(&FileMessage::End { id: 1 }),
        ])
        .await;
        assert_eq!(
            h.sent(),
            vec![FileMessage::Abort {
                id: 1,
                reason: AbortReason::Protocol
            }]
        );
        assert!(h.files().is_empty());
    }

    #[tokio::test]
    async fn client_abort_removes_the_part_file() {
        let mut h = Harness::new(true);
        h.feed(vec![start(1, "a.bin", 10), chunk(b"abc")]).await;
        assert_eq!(h.files().len(), 2); // placeholder + part
        h.feed(vec![text(&FileMessage::Abort {
            id: 1,
            reason: AbortReason::Cancelled,
        })])
        .await;
        assert!(h.files().is_empty());
        assert!(h.sent().is_empty());
    }

    #[tokio::test]
    async fn start_over_active_aborts_the_old_one() {
        let mut h = Harness::new(true);
        h.feed(vec![
            start(1, "old.bin", 10),
            chunk(b"abc"),
            start(2, "new.bin", 2),
            chunk(b"hi"),
            text(&FileMessage::End { id: 2 }),
        ])
        .await;
        assert_eq!(
            h.sent(),
            vec![
                FileMessage::Abort {
                    id: 1,
                    reason: AbortReason::Protocol
                },
                FileMessage::Saved {
                    id: 2,
                    name: "new.bin".into()
                }
            ]
        );
        assert_eq!(h.files(), vec!["new.bin".to_string()]);
    }

    #[tokio::test]
    async fn chunk_without_active_receive_is_dropped() {
        let mut h = Harness::new(true);
        h.feed(vec![chunk(b"abc")]).await;
        assert!(h.sent().is_empty());
        assert!(h.files().is_empty());
    }

    #[tokio::test]
    async fn channel_loss_mid_transfer_removes_the_part_file() {
        let h = Harness::new(true);
        let dir = h.dir.clone();
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(run(
            Arc::clone(&h.fake) as Arc<dyn FileChannel>,
            rx,
            Some(dir.clone()),
        ));
        tx.send(start(1, "a.bin", 10)).await.unwrap();
        tx.send(chunk(b"abc")).await.unwrap();
        // Wait until the transfer has really started on disk.
        for _ in 0..200 {
            if dir.join("a.bin.rcdesk-part").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(dir.join("a.bin.rcdesk-part").exists());
        drop(tx);
        task.await.unwrap();
        assert!(h.files().is_empty());
    }

    #[tokio::test]
    async fn aborting_the_task_removes_the_part_file() {
        let h = Harness::new(true);
        let dir = h.dir.clone();
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(run(
            Arc::clone(&h.fake) as Arc<dyn FileChannel>,
            rx,
            Some(dir.clone()),
        ));
        tx.send(start(1, "a.bin", 10)).await.unwrap();
        for _ in 0..200 {
            if dir.join("a.bin.rcdesk-part").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(dir.join("a.bin.rcdesk-part").exists());
        task.abort();
        let _ = task.await;
        assert!(h.files().is_empty());
    }

    #[tokio::test]
    async fn disabled_answers_every_start_with_disabled_and_creates_nothing() {
        let mut h = Harness::new(false);
        h.feed(vec![start(5, "a.txt", 3), chunk(b"abc")]).await;
        assert_eq!(
            h.sent(),
            vec![FileMessage::Abort {
                id: 5,
                reason: AbortReason::Disabled
            }]
        );
        assert!(!h.dir.exists());
    }
}
