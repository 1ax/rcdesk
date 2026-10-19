//! File transfer over the `file` data channel (slice 3.4, see
//! `proto::file` for the wire protocol). Client -> host (3.4a): the host
//! receives a file into the user's Downloads folder. Host -> client (3.4c):
//! files the host user copied (see `crate::clipboard`) are offered to the
//! client and, on its `Fetch`, streamed to it.
//!
//! The channel is served by one task per session (`run`), fed by the
//! transport's dedicated `file` mpsc (`transport::FileChannelMessage`)
//! instead of `SessionEvent::DataChannelMessage`, so disk writes never run
//! inside `signaling::handle_session_event`. Replies go out through the
//! `FileChannel` trait so the logic is testable without WebRTC.
//!
//! Receiving and sending share the one task: `run` selects over the incoming
//! channel messages and the offer commands, while a download runs in its own
//! spawned task (`Outgoing`, cancelled through a flag, aborted on drop), so a
//! long download never blocks incoming messages (client `Abort`, a parallel
//! upload, ...). Download ids start at `proto::file::HOST_ID_BASE`, upload
//! ids (client) at 1, so `Abort` ids never clash.
//!
//! A copied folder is offered as `<folder>.zip`: a STORED (uncompressed) zip
//! assembled on the fly while streaming (`zip`), its exact size known up front.
//!
//! A client may also upload a folder (3.4e): a sequence of single-file
//! transfers whose `Start` names a folder number and a relative directory.
//! The host recreates the tree under a fresh, de-duplicated root directory
//! inside Downloads; every path component is sanitized, so nothing can land
//! outside Downloads.
//!
//! Disk safety: data is written to `<name>.rcdesk-part`; the final name is
//! reserved up front with `create_new` (an empty placeholder, so two
//! transfers can never pick the same name) and the part file replaces it on
//! a successful `End`. Anything else (abort, protocol error, channel loss,
//! task cancellation) removes both files.
//!
//! Privacy: file names are logged at `debug` only, never file content.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use bytes::BytesMut;
use proto::file::{AbortReason, FileMessage, OfferedFile, HOST_ID_BASE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannel;

use crate::transport::FileChannelMessage;

mod zip;

#[cfg(target_os = "windows")]
pub mod windows;

/// Extension of the in-progress file next to the final one.
const PART_SUFFIX: &str = ".rcdesk-part";

/// Longest sanitized file name, in UTF-8 bytes.
const MAX_NAME_BYTES: usize = 200;

/// An extension longer than this (including the dot) is not preserved when
/// a long name is truncated.
const MAX_KEPT_EXT_BYTES: usize = 20;

/// Where incoming files land: the user's Downloads folder. `console_user`
/// (Windows full mode, helper runs as SYSTEM) takes the folder of the user
/// logged on to the helper's session; ignored on other platforms.
pub fn receive_dir(console_user: bool) -> anyhow::Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        windows::downloads_dir(console_user)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = console_user;
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| anyhow::anyhow!("HOME is not set, cannot locate Downloads"))?;
        Ok(PathBuf::from(home).join("Downloads"))
    }
}

/// Size of one download chunk (the protocol maximum).
const CHUNK_BYTES: usize = 64 * 1024;

/// A download waits while the channel has more than this many bytes queued.
const SEND_HIGH_WATER: usize = 1024 * 1024;

/// How long a download sleeps between checks of the queued byte count.
const BACKPRESSURE_POLL: Duration = Duration::from_millis(5);

/// What the clipboard watcher tells the `file` task (slice 3.4c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesCommand {
    /// The host user copied these paths: offer them to the client.
    Offer(Vec<PathBuf>),
    /// The copied files are gone from the clipboard: withdraw the offer.
    Clear,
}

/// The sending half of the `file` channel as the transfer logic sees it.
#[async_trait::async_trait]
pub trait FileChannel: Send + Sync {
    async fn send_text(&self, text: &str) -> anyhow::Result<()>;
    async fn send_binary(&self, data: &[u8]) -> anyhow::Result<()>;
    /// Bytes handed to the channel that the peer has not yet acknowledged.
    async fn outstanding_bytes(&self) -> usize;
}

#[async_trait::async_trait]
impl FileChannel for Arc<dyn DataChannel> {
    async fn send_text(&self, text: &str) -> anyhow::Result<()> {
        DataChannel::send_text(self.as_ref(), text).await?;
        Ok(())
    }

    async fn send_binary(&self, data: &[u8]) -> anyhow::Result<()> {
        DataChannel::send(self.as_ref(), BytesMut::from(data)).await?;
        Ok(())
    }

    async fn outstanding_bytes(&self) -> usize {
        DataChannel::outstanding_bytes(self.as_ref())
            .await
            .unwrap_or(0)
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
/// with its `.rcdesk-part` file, and returns them owned by an
/// [`ActiveReceive`].
///
/// Cancel-safe: all file creation runs inside ONE blocking task that returns
/// the already-guarding `ActiveReceive`. If the awaiting future is dropped
/// (task abort, session close), the blocking task still finishes and its
/// result is dropped, whose `Drop` removes both files. Doing this with several
/// awaited `tokio::fs` calls would leak files created in between.
async fn create_unique(
    dir: &Path,
    name: &str,
    id: u32,
    size: u64,
) -> std::io::Result<ActiveReceive> {
    let dir = dir.to_path_buf();
    let name = name.to_string();
    tokio::task::spawn_blocking(move || create_unique_blocking(&dir, &name, id, size))
        .await
        .map_err(std::io::Error::other)?
}

fn create_unique_blocking(
    dir: &Path,
    name: &str,
    id: u32,
    size: u64,
) -> std::io::Result<ActiveReceive> {
    use std::fs::OpenOptions;
    for n in 0..10_000u32 {
        let candidate = with_suffix(name, n);
        let final_path = dir.join(&candidate);
        let part_path = dir.join(format!("{candidate}{PART_SUFFIX}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&final_path)
        {
            Ok(_placeholder) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
        {
            Ok(file) => {
                return Ok(ActiveReceive {
                    id,
                    size,
                    written: 0,
                    file: Some(tokio::fs::File::from_std(file)),
                    part_path,
                    final_path,
                    final_name: candidate,
                    finished: false,
                })
            }
            Err(e) => {
                let _ = std::fs::remove_file(&final_path);
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

/// Longest directory path (in components) accepted for a folder upload.
const MAX_DIR_COMPONENTS: usize = 32;

/// Splits the `dir` of a folder upload into sanitized components. `None`
/// when there are none or too many (the file is then saved flat).
fn split_dir(dir: &str) -> Option<Vec<String>> {
    let comps: Vec<String> = dir
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .map(sanitize_file_name)
        .collect();
    (!comps.is_empty() && comps.len() <= MAX_DIR_COMPONENTS).then_some(comps)
}

/// Creates a fresh directory `name` in `parent`, trying `name (1)`, ... when
/// taken. `create_dir` (not `_all`) fails with `AlreadyExists` atomically.
async fn create_unique_dir(parent: &Path, name: &str) -> std::io::Result<PathBuf> {
    for n in 0..10_000u32 {
        let path = parent.join(with_suffix(name, n));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free directory name",
    ))
}

/// One transfer in progress. Dropping it without `finished` removes the
/// part file and the reserved placeholder (also on task cancellation); see
/// `create_unique` for why it is built inside the blocking task.
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
    /// Uploaded folders of this session: client folder number -> the root
    /// directory chosen for it on disk (slice 3.4e).
    folders: HashMap<u32, PathBuf>,
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

    async fn on_control(&mut self, msg: FileMessage) {
        match msg {
            FileMessage::Start {
                id,
                name,
                size,
                folder,
                dir,
            } => self.on_start(id, name, size, folder, dir).await,
            FileMessage::End { id } => self.on_end(id).await,
            FileMessage::Abort { id, .. } => {
                if self.active.as_ref().is_some_and(|a| a.id == id) {
                    tracing::debug!(id, "file receive aborted by peer");
                    self.active = None;
                }
            }
            FileMessage::Saved { .. } | FileMessage::Offer { .. } | FileMessage::Fetch { .. } => {
                tracing::trace!("message not meant for the upload receiver, ignoring");
            }
        }
    }

    /// Directory for the file of a `Start`: Downloads itself for an ordinary
    /// upload, or (folder upload) a directory inside the folder's root,
    /// created on demand. The result is always inside `base`.
    async fn target_dir(
        &mut self,
        base: &Path,
        folder: Option<u32>,
        dir: Option<&str>,
    ) -> std::io::Result<PathBuf> {
        let (Some(folder), Some(dir)) = (folder, dir) else {
            if folder.is_some() || dir.is_some() {
                tracing::debug!("folder and dir must come together, saving flat");
            }
            return Ok(base.to_path_buf());
        };
        let Some(components) = split_dir(dir) else {
            tracing::debug!("unusable folder dir, saving flat");
            return Ok(base.to_path_buf());
        };
        let root = match self.folders.get(&folder) {
            Some(root) => root.clone(),
            None => {
                let root = create_unique_dir(base, &components[0]).await?;
                self.folders.insert(folder, root.clone());
                root
            }
        };
        let mut target = root;
        for comp in &components[1..] {
            target.push(comp);
        }
        debug_assert!(target.starts_with(base));
        if !target.starts_with(base) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "folder path escapes the download directory",
            ));
        }
        tokio::fs::create_dir_all(&target).await?;
        Ok(target)
    }

    async fn on_start(
        &mut self,
        id: u32,
        name: String,
        size: u64,
        folder: Option<u32>,
        rel_dir: Option<String>,
    ) {
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
            Ok(()) => match self.target_dir(&dir, folder, rel_dir.as_deref()).await {
                Ok(target) => create_unique(&target, &clean, id, size).await,
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        match created {
            Ok(active) => {
                tracing::debug!(id, name = %active.final_name, size, "file receive started");
                self.active = Some(active);
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

/// One file of the current offer.
struct OfferEntry {
    path: PathBuf,
    name: String,
    /// File size, or the exact zip size for a folder.
    size: u64,
    kind: EntryKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EntryKind {
    File,
    /// Streamed as a STORED zip, re-walked at send time.
    Folder,
}

/// A running download of one offer. Dropping it stops the task.
struct Outgoing {
    handle: tokio::task::JoinHandle<()>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Outgoing {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Outgoing {
    fn is_running(&self) -> bool {
        !self.handle.is_finished()
    }
}

/// Host -> client direction: offers and downloads.
struct Downloader {
    channel: Arc<dyn FileChannel>,
    /// Id and files of the latest offer only.
    list: Option<(u32, Vec<OfferEntry>)>,
    next_list_id: u32,
    /// Next download file id, shared with the running download task.
    next_file_id: Arc<AtomicU32>,
    sending: Option<Outgoing>,
}

async fn send_msg(channel: &dyn FileChannel, msg: &FileMessage) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    channel.send_text(&json).await
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
}

impl Downloader {
    fn new(channel: Arc<dyn FileChannel>) -> Self {
        Self {
            channel,
            list: None,
            next_list_id: 1,
            next_file_id: Arc::new(AtomicU32::new(HOST_ID_BASE)),
            sending: None,
        }
    }

    async fn command(&mut self, cmd: FilesCommand) {
        match cmd {
            FilesCommand::Offer(paths) => self.offer(paths).await,
            FilesCommand::Clear => self.clear().await,
        }
    }

    async fn clear(&mut self) {
        if self.list.take().is_some() {
            let list_id = self.take_list_id();
            let msg = FileMessage::Offer {
                list_id,
                files: vec![],
                skipped_folders: 0,
            };
            if let Err(err) = send_msg(self.channel.as_ref(), &msg).await {
                tracing::debug!(?err, "failed to withdraw file offer");
            }
        }
    }

    fn take_list_id(&mut self) -> u32 {
        let id = self.next_list_id;
        self.next_list_id = self.next_list_id.wrapping_add(1);
        id
    }

    async fn offer(&mut self, paths: Vec<PathBuf>) {
        let mut entries = Vec::new();
        // Folders are offered now (as zip); the protocol field stays 0.
        let skipped_folders = 0u32;
        for path in paths {
            match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.is_file() => entries.push(OfferEntry {
                    name: file_name_of(&path),
                    size: meta.len(),
                    path,
                    kind: EntryKind::File,
                }),
                Ok(meta) if meta.is_dir() => {
                    let root = path.clone();
                    match tokio::task::spawn_blocking(move || zip::walk(&root)).await {
                        Ok(Ok(manifest)) => entries.push(OfferEntry {
                            name: format!("{}.zip", file_name_of(&path)),
                            size: manifest.zip_size(),
                            path,
                            kind: EntryKind::Folder,
                        }),
                        Ok(Err(err)) => {
                            tracing::debug!(path = %path.display(), ?err, "cannot walk folder, skipping")
                        }
                        Err(err) => {
                            tracing::debug!(path = %path.display(), ?err, "folder walk failed, skipping")
                        }
                    }
                }
                Ok(_) => tracing::debug!(path = %path.display(), "not a regular file, skipping"),
                Err(err) => {
                    tracing::debug!(path = %path.display(), ?err, "cannot stat copied path, skipping")
                }
            }
        }
        if entries.is_empty() && skipped_folders == 0 {
            // Nothing offerable: same as no files at all.
            self.clear().await;
            return;
        }
        let list_id = self.take_list_id();
        let files = entries
            .iter()
            .map(|e| OfferedFile {
                name: e.name.clone(),
                size: e.size,
            })
            .collect();
        tracing::debug!(
            list_id,
            files = entries.len(),
            skipped_folders,
            "offering copied files"
        );
        self.list = Some((list_id, entries));
        let msg = FileMessage::Offer {
            list_id,
            files,
            skipped_folders,
        };
        if let Err(err) = send_msg(self.channel.as_ref(), &msg).await {
            tracing::debug!(?err, "failed to send file offer");
        }
    }

    fn fetch(&mut self, list_id: u32) {
        if self.sending.as_ref().is_some_and(Outgoing::is_running) {
            tracing::debug!(list_id, "fetch while a download is running, ignoring");
            return;
        }
        let Some((current, entries)) = self.list.as_ref() else {
            tracing::debug!(list_id, "fetch without an offer, ignoring");
            return;
        };
        if *current != list_id || entries.is_empty() {
            tracing::debug!(list_id, "fetch for a stale or empty offer, ignoring");
            return;
        }
        let entries: Vec<OfferEntry> = entries
            .iter()
            .map(|e| OfferEntry {
                path: e.path.clone(),
                name: e.name.clone(),
                size: e.size,
                kind: e.kind,
            })
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(send_files(
            Arc::clone(&self.channel),
            entries,
            Arc::clone(&self.next_file_id),
            Arc::clone(&cancel),
        ));
        self.sending = Some(Outgoing { handle, cancel });
    }

    /// The client aborted transfer `id`. Ids from `HOST_ID_BASE` up are
    /// downloads; while one runs, the current file and the rest of the list
    /// are dropped. (The id may name a file the host has already left
    /// behind -- the client cannot know -- so it is not compared further.)
    fn client_abort(&mut self, id: u32, reason: AbortReason) {
        let Some(out) = self.sending.as_ref().filter(|o| o.is_running()) else {
            return;
        };
        if id >= HOST_ID_BASE && matches!(reason, AbortReason::Cancelled | AbortReason::Protocol) {
            tracing::debug!(id, "download aborted by client");
            out.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Streams `entries` over `channel`, one file after another, until done,
/// cancelled or the channel fails.
async fn send_files(
    channel: Arc<dyn FileChannel>,
    entries: Vec<OfferEntry>,
    next_file_id: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
) {
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let id = next_file_id.fetch_add(1, Ordering::Relaxed);
        match send_one(channel.as_ref(), id, &entry, &cancel).await {
            Ok(()) => {}
            Err(err) => {
                tracing::debug!(?err, id, "download stopped, channel failed");
                return;
            }
        }
    }
}

/// Sends one file. `Err` only when the channel itself failed; a file-level
/// problem is reported to the client with `Abort { Io }` and is `Ok`.
async fn send_one(
    channel: &dyn FileChannel,
    id: u32,
    entry: &OfferEntry,
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    if entry.kind == EntryKind::Folder {
        return send_folder(channel, id, entry, cancel).await;
    }
    let opened = async {
        let file = tokio::fs::File::open(&entry.path).await?;
        let meta = file.metadata().await?;
        if !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        Ok((file, meta.len()))
    }
    .await;
    let (mut file, size) = match opened {
        Ok(v) => v,
        Err(err) => {
            tracing::debug!(path = %entry.path.display(), ?err, "cannot open file for download");
            // The client has no other way to learn which file failed: the
            // `Start` carries the offered name and size.
            send_msg(
                channel,
                &FileMessage::Start {
                    id,
                    name: entry.name.clone(),
                    size: entry.size,
                    folder: None,
                    dir: None,
                },
            )
            .await?;
            send_msg(
                channel,
                &FileMessage::Abort {
                    id,
                    reason: AbortReason::Io,
                },
            )
            .await?;
            return Ok(());
        }
    };
    send_msg(
        channel,
        &FileMessage::Start {
            id,
            name: entry.name.clone(),
            size,
            folder: None,
            dir: None,
        },
    )
    .await?;
    let mut remaining = size;
    let mut buf = vec![0u8; CHUNK_BYTES];
    while remaining > 0 {
        while channel.outstanding_bytes().await > SEND_HIGH_WATER {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            tokio::time::sleep(BACKPRESSURE_POLL).await;
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let want = remaining.min(CHUNK_BYTES as u64) as usize;
        if let Err(err) = file.read_exact(&mut buf[..want]).await {
            tracing::debug!(?err, id, "download read failed");
            send_msg(
                channel,
                &FileMessage::Abort {
                    id,
                    reason: AbortReason::Io,
                },
            )
            .await?;
            return Ok(());
        }
        channel.send_binary(&buf[..want]).await?;
        remaining -= want as u64;
    }
    send_msg(channel, &FileMessage::End { id }).await?;
    tracing::info!(size, "file sent");
    Ok(())
}

/// Sends a folder as a STORED zip. The folder is walked afresh, so the
/// `Start` size is the exact size of the archive that follows (it may differ
/// from the offered one). Same error/cancel contract as `send_one`.
async fn send_folder(
    channel: &dyn FileChannel,
    id: u32,
    entry: &OfferEntry,
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let root = entry.path.clone();
    let manifest = match tokio::task::spawn_blocking(move || zip::walk(&root)).await {
        Ok(Ok(m)) => Some(m),
        Ok(Err(err)) => {
            tracing::debug!(path = %entry.path.display(), ?err, "cannot walk folder for download");
            None
        }
        Err(err) => {
            tracing::debug!(?err, "folder walk task failed");
            None
        }
    };
    let abort_io = FileMessage::Abort {
        id,
        reason: AbortReason::Io,
    };
    let Some(manifest) = manifest else {
        send_msg(
            channel,
            &FileMessage::Start {
                id,
                name: entry.name.clone(),
                size: entry.size,
                folder: None,
                dir: None,
            },
        )
        .await?;
        send_msg(channel, &abort_io).await?;
        return Ok(());
    };
    let size = manifest.zip_size();
    send_msg(
        channel,
        &FileMessage::Start {
            id,
            name: entry.name.clone(),
            size,
            folder: None,
            dir: None,
        },
    )
    .await?;
    let mut stream = match zip::ZipStream::new(manifest) {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(?err, id, "folder too large for download");
            send_msg(channel, &abort_io).await?;
            return Ok(());
        }
    };
    let mut sent = 0u64;
    loop {
        while channel.outstanding_bytes().await > SEND_HIGH_WATER {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            tokio::time::sleep(BACKPRESSURE_POLL).await;
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        match stream.next_chunk().await {
            Ok(Some(chunk)) => {
                channel.send_binary(&chunk).await?;
                sent += chunk.len() as u64;
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(?err, id, "folder download read failed");
                send_msg(channel, &abort_io).await?;
                return Ok(());
            }
        }
    }
    if sent != size {
        tracing::debug!(sent, size, id, "zip size mismatch");
        send_msg(channel, &abort_io).await?;
        return Ok(());
    }
    send_msg(channel, &FileMessage::End { id }).await?;
    tracing::info!(size, "folder sent");
    Ok(())
}

/// Both directions of the `file` channel for one session.
struct Session {
    receiver: Receiver,
    downloader: Downloader,
}

impl Session {
    async fn on_message(&mut self, msg: FileChannelMessage) {
        if msg.is_string {
            match serde_json::from_slice::<FileMessage>(&msg.data) {
                Ok(FileMessage::Fetch { list_id }) => self.downloader.fetch(list_id),
                Ok(m) => {
                    if let FileMessage::Abort { id, reason } = &m {
                        self.downloader.client_abort(*id, *reason);
                    }
                    self.receiver.on_control(m).await;
                }
                Err(err) => tracing::warn!(?err, "invalid file channel message, ignoring"),
            }
        } else {
            self.receiver.on_chunk(&msg.data).await;
        }
    }
}

/// Serves the `file` channel for one session until `rx` closes (the channel
/// or the session ended) or the task is aborted; either way an unfinished
/// transfer is cleaned up and a running download stopped. `dir` is the
/// receive folder, or `None` when file transfer is disabled -- every `Start`
/// is then answered with `Abort { Disabled }` and nothing is created.
/// `commands` carries the clipboard watcher's file offers (host -> client);
/// when it closes the task keeps serving the channel.
pub async fn run(
    channel: Arc<dyn FileChannel>,
    mut rx: mpsc::Receiver<FileChannelMessage>,
    dir: Option<PathBuf>,
    mut commands: mpsc::Receiver<FilesCommand>,
) {
    let mut session = Session {
        receiver: Receiver {
            channel: Arc::clone(&channel),
            dir,
            active: None,
            folders: HashMap::new(),
        },
        downloader: Downloader::new(channel),
    };
    let mut commands_open = true;
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => session.on_message(msg).await,
                None => break,
            },
            cmd = commands.recv(), if commands_open => match cmd {
                Some(cmd) => session.downloader.command(cmd).await,
                None => commands_open = false,
            },
        }
    }
    if session.receiver.active.take().is_some() {
        tracing::debug!("file channel ended mid-transfer, partial file removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::AtomicU64;
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Sent {
        Msg(FileMessage),
        Chunk(Vec<u8>),
    }

    struct FakeChannel {
        sent: Mutex<Vec<Sent>>,
        outstanding: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl FileChannel for FakeChannel {
        async fn send_text(&self, text: &str) -> anyhow::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push(Sent::Msg(serde_json::from_str(text).unwrap()));
            Ok(())
        }

        async fn send_binary(&self, data: &[u8]) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(Sent::Chunk(data.to_vec()));
            Ok(())
        }

        async fn outstanding_bytes(&self) -> usize {
            self.outstanding.load(Ordering::Relaxed)
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
            folder: None,
            dir: None,
        })
    }

    struct Harness {
        session: Session,
        fake: Arc<FakeChannel>,
        dir: PathBuf,
    }

    impl Harness {
        fn new(enabled: bool) -> Self {
            let dir = scratch_dir();
            let fake = Arc::new(FakeChannel {
                sent: Mutex::new(vec![]),
                outstanding: std::sync::atomic::AtomicUsize::new(0),
            });
            let channel = Arc::clone(&fake) as Arc<dyn FileChannel>;
            let session = Session {
                receiver: Receiver {
                    channel: Arc::clone(&channel),
                    dir: enabled.then(|| dir.clone()),
                    active: None,
                    folders: HashMap::new(),
                },
                downloader: Downloader::new(channel),
            };
            Self { session, fake, dir }
        }

        async fn feed(&mut self, msgs: Vec<FileChannelMessage>) {
            for m in msgs {
                self.session.on_message(m).await;
            }
        }

        /// Control messages only (chunks left out).
        fn sent(&self) -> Vec<FileMessage> {
            self.log()
                .into_iter()
                .filter_map(|s| match s {
                    Sent::Msg(m) => Some(m),
                    Sent::Chunk(_) => None,
                })
                .collect()
        }

        /// Everything sent, in order.
        fn log(&self) -> Vec<Sent> {
            self.fake.sent.lock().unwrap().clone()
        }

        async fn command(&mut self, cmd: FilesCommand) {
            self.session.downloader.command(cmd).await;
        }

        fn set_outstanding(&self, n: usize) {
            self.fake.outstanding.store(n, Ordering::Relaxed);
        }

        /// Waits until the running download task has finished.
        async fn wait_download_done(&self) {
            for _ in 0..1000 {
                if !self
                    .session
                    .downloader
                    .sending
                    .as_ref()
                    .is_some_and(Outgoing::is_running)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("download did not finish");
        }

        /// Waits until `n` entries are in the log.
        async fn wait_log_len(&self, n: usize) {
            for _ in 0..1000 {
                if self.log().len() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("log did not reach {n}: {:?}", self.log());
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

    fn start_in(id: u32, name: &str, folder: Option<u32>, dir: Option<&str>) -> FileChannelMessage {
        text(&FileMessage::Start {
            id,
            name: name.to_string(),
            size: 1,
            folder,
            dir: dir.map(str::to_string),
        })
    }

    async fn upload_in(
        h: &mut Harness,
        id: u32,
        name: &str,
        folder: Option<u32>,
        dir: Option<&str>,
    ) {
        h.feed(vec![
            start_in(id, name, folder, dir),
            chunk(b"x"),
            text(&FileMessage::End { id }),
        ])
        .await;
    }

    #[tokio::test]
    async fn folder_files_keep_their_tree() {
        let mut h = Harness::new(true);
        upload_in(&mut h, 1, "a.txt", Some(1), Some("Фото")).await;
        upload_in(&mut h, 2, "b.txt", Some(1), Some("Фото/2024")).await;
        assert!(h.dir.join("Фото/a.txt").is_file());
        assert!(h.dir.join("Фото/2024/b.txt").is_file());
        assert_eq!(h.files(), vec!["Фото".to_string()]);
        let saved: Vec<String> = h
            .sent()
            .into_iter()
            .map(|m| match m {
                FileMessage::Saved { name, .. } => name,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(saved, vec!["a.txt", "b.txt"]);
    }

    #[tokio::test]
    async fn existing_root_gets_a_suffix_once_per_folder() {
        let mut h = Harness::new(true);
        std::fs::create_dir_all(h.dir.join("Фото")).unwrap();
        upload_in(&mut h, 1, "a.txt", Some(1), Some("Фото")).await;
        upload_in(&mut h, 2, "b.txt", Some(1), Some("Фото/sub")).await;
        upload_in(&mut h, 3, "c.txt", Some(2), Some("Фото")).await;
        assert!(h.dir.join("Фото (1)/a.txt").is_file());
        assert!(h.dir.join("Фото (1)/sub/b.txt").is_file());
        assert!(h.dir.join("Фото (2)/c.txt").is_file());
        assert!(!h.dir.join("Фото/a.txt").exists());
    }

    #[tokio::test]
    async fn later_files_use_the_folder_root_from_the_map() {
        let mut h = Harness::new(true);
        upload_in(&mut h, 1, "a.txt", Some(5), Some("Один")).await;
        // The first component of a later dir is ignored.
        upload_in(&mut h, 2, "b.txt", Some(5), Some("Другой/x")).await;
        assert!(h.dir.join("Один/a.txt").is_file());
        assert!(h.dir.join("Один/x/b.txt").is_file());
        assert!(!h.dir.join("Другой").exists());
    }

    #[tokio::test]
    async fn escaping_dirs_stay_inside_downloads() {
        let outer = scratch_dir();
        let dir = outer.join("Downloads");
        std::fs::create_dir_all(&dir).unwrap();
        let fake = Arc::new(FakeChannel {
            sent: Mutex::new(vec![]),
            outstanding: std::sync::atomic::AtomicUsize::new(0),
        });
        let channel = Arc::clone(&fake) as Arc<dyn FileChannel>;
        let mut session = Session {
            receiver: Receiver {
                channel: Arc::clone(&channel),
                dir: Some(dir.clone()),
                active: None,
                folders: HashMap::new(),
            },
            downloader: Downloader::new(channel),
        };
        let dirs = ["../x", "/etc", "C:\\Windows", "a/../../b", ".."];
        for (i, d) in dirs.iter().enumerate() {
            let id = i as u32 + 1;
            for m in [
                start_in(id, "f.txt", Some(id), Some(d)),
                chunk(b"x"),
                text(&FileMessage::End { id }),
            ] {
                session.on_message(m).await;
            }
        }
        for root in session.receiver.folders.values() {
            assert!(root.starts_with(&dir), "{root:?}");
        }
        assert_eq!(session.receiver.folders.len(), dirs.len());
        // Nothing was created next to Downloads.
        let outside: Vec<_> = std::fs::read_dir(&outer)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(outside, vec!["Downloads".to_string()]);
        // Every saved file is inside Downloads.
        fn walk(p: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(p).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
        let mut files = vec![];
        walk(&dir, &mut files);
        assert_eq!(files.len(), dirs.len());
        assert!(files.iter().all(|f| f.starts_with(&dir)));
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn split_dir_sanitizes_and_limits() {
        assert_eq!(
            split_dir("a/./b//c"),
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(
            split_dir("a\\b"),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(split_dir(".."), Some(vec!["file".to_string()]));
        assert_eq!(
            split_dir("a/../../b"),
            Some(vec!["a".into(), "file".into(), "file".into(), "b".into()])
        );
        assert_eq!(
            split_dir("C:\\Windows"),
            Some(vec!["C_".to_string(), "Windows".to_string()])
        );
        assert_eq!(split_dir(""), None);
        assert_eq!(split_dir("/./"), None);
        assert!(split_dir(&vec!["a"; 32].join("/")).is_some());
        assert_eq!(split_dir(&vec!["a"; 33].join("/")), None);
    }

    #[tokio::test]
    async fn unusable_folder_fields_save_flat() {
        let mut h = Harness::new(true);
        let deep = vec!["d"; 33].join("/");
        upload_in(&mut h, 1, "a.txt", Some(1), Some(&deep)).await;
        upload_in(&mut h, 2, "b.txt", Some(2), None).await;
        upload_in(&mut h, 3, "c.txt", None, Some("Фото")).await;
        assert_eq!(
            h.files(),
            vec![
                "a.txt".to_string(),
                "b.txt".to_string(),
                "c.txt".to_string()
            ]
        );
        assert!(h.session.receiver.folders.is_empty());
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
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let task = tokio::spawn(run(
            Arc::clone(&h.fake) as Arc<dyn FileChannel>,
            rx,
            Some(dir.clone()),
            cmd_rx,
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
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let task = tokio::spawn(run(
            Arc::clone(&h.fake) as Arc<dyn FileChannel>,
            rx,
            Some(dir.clone()),
            cmd_rx,
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
        // The cleanup may finish slightly later on a blocking thread.
        for _ in 0..200 {
            if h.files().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(h.files().is_empty());
    }

    #[tokio::test]
    async fn cancelling_create_unique_midway_leaves_no_files() {
        let h = Harness::new(true);
        std::fs::create_dir_all(&h.dir).unwrap();
        for _ in 0..20 {
            let _ = tokio::time::timeout(
                std::time::Duration::from_micros(1),
                create_unique(&h.dir, "a.bin", 1, 10),
            )
            .await;
            for _ in 0..200 {
                if h.files().is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(h.files().is_empty(), "leaked: {:?}", h.files());
        }
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

    // ---- host -> client (3.4c) ----

    fn msg(m: FileMessage) -> Sent {
        Sent::Msg(m)
    }

    fn put(h: &Harness, name: &str, data: &[u8]) -> PathBuf {
        std::fs::create_dir_all(&h.dir).unwrap();
        let p = h.dir.join(name);
        std::fs::write(&p, data).unwrap();
        p
    }

    fn fetch(list_id: u32) -> FileChannelMessage {
        text(&FileMessage::Fetch { list_id })
    }

    #[tokio::test]
    async fn offer_lists_files_and_folders_as_zip() {
        let mut h = Harness::new(true);
        let file = put(&h, "a.txt", b"hello");
        let sub = h.dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let missing = h.dir.join("gone.txt");
        h.command(FilesCommand::Offer(vec![file, sub.clone(), missing]))
            .await;
        assert_eq!(
            h.sent(),
            vec![FileMessage::Offer {
                list_id: 1,
                files: vec![
                    OfferedFile {
                        name: "a.txt".into(),
                        size: 5
                    },
                    OfferedFile {
                        name: "sub.zip".into(),
                        size: zip::walk(&sub).unwrap().zip_size()
                    }
                ],
                skipped_folders: 0,
            }]
        );
    }

    #[tokio::test]
    async fn fetch_streams_a_folder_as_zip_after_a_file() {
        let mut h = Harness::new(true);
        let file = put(&h, "a.txt", b"hello");
        let folder = h.dir.join("pack");
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        std::fs::write(folder.join("x.bin"), vec![9u8; CHUNK_BYTES + 5]).unwrap();
        let zip_size = zip::walk(&folder).unwrap().zip_size();
        h.command(FilesCommand::Offer(vec![file, folder])).await;
        assert_eq!(
            h.sent()[0],
            FileMessage::Offer {
                list_id: 1,
                files: vec![
                    OfferedFile {
                        name: "a.txt".into(),
                        size: 5
                    },
                    OfferedFile {
                        name: "pack.zip".into(),
                        size: zip_size
                    }
                ],
                skipped_folders: 0,
            }
        );
        h.feed(vec![fetch(1)]).await;
        h.wait_download_done().await;
        let log = h.log();
        assert_eq!(
            log[1],
            msg(FileMessage::Start {
                id: HOST_ID_BASE,
                name: "a.txt".into(),
                size: 5,
                folder: None,
                dir: None
            })
        );
        assert_eq!(log[3], msg(FileMessage::End { id: HOST_ID_BASE }));
        assert_eq!(
            log[4],
            msg(FileMessage::Start {
                id: HOST_ID_BASE + 1,
                name: "pack.zip".into(),
                size: zip_size,
                folder: None,
                dir: None
            })
        );
        let mut total = 0u64;
        let mut first = Vec::new();
        for item in &log[5..log.len() - 1] {
            match item {
                Sent::Chunk(c) => {
                    if first.is_empty() {
                        first = c.clone();
                    }
                    total += c.len() as u64;
                }
                other => panic!("expected chunk, got {other:?}"),
            }
        }
        assert_eq!(total, zip_size);
        assert_eq!(&first[..4], b"PK\x03\x04");
        assert_eq!(
            log.last().unwrap(),
            &msg(FileMessage::End {
                id: HOST_ID_BASE + 1
            })
        );
    }

    #[tokio::test]
    async fn folder_vanished_after_offer_is_aborted() {
        let mut h = Harness::new(true);
        let folder = h.dir.join("pack");
        std::fs::create_dir_all(&folder).unwrap();
        h.command(FilesCommand::Offer(vec![folder.clone()])).await;
        std::fs::remove_dir_all(&folder).unwrap();
        h.feed(vec![fetch(1)]).await;
        h.wait_download_done().await;
        let sent = h.sent();
        assert!(matches!(&sent[1], FileMessage::Start { name, .. } if name == "pack.zip"));
        assert_eq!(
            sent[2],
            FileMessage::Abort {
                id: HOST_ID_BASE,
                reason: AbortReason::Io
            }
        );
    }

    #[tokio::test]
    async fn clear_withdraws_only_an_existing_offer() {
        let mut h = Harness::new(true);
        h.command(FilesCommand::Clear).await;
        assert!(h.sent().is_empty());
        let file = put(&h, "a.txt", b"x");
        h.command(FilesCommand::Offer(vec![file])).await;
        h.command(FilesCommand::Clear).await;
        assert_eq!(
            h.sent()[1],
            FileMessage::Offer {
                list_id: 2,
                files: vec![],
                skipped_folders: 0
            }
        );
        // The withdrawn list can no longer be fetched.
        h.feed(vec![fetch(1)]).await;
        assert_eq!(h.sent().len(), 2);
    }

    #[tokio::test]
    async fn fetch_sends_start_chunks_end_with_content() {
        let mut h = Harness::new(true);
        let big: Vec<u8> = (0..CHUNK_BYTES * 2 + 10).map(|i| (i % 251) as u8).collect();
        let a = put(&h, "big.bin", &big);
        let b = put(&h, "empty.txt", b"");
        h.command(FilesCommand::Offer(vec![a, b])).await;
        h.feed(vec![fetch(1)]).await;
        h.wait_download_done().await;
        let log = h.log();
        assert_eq!(log.len(), 1 + 1 + 3 + 1 + 2);
        assert_eq!(
            log[1],
            msg(FileMessage::Start {
                id: HOST_ID_BASE,
                name: "big.bin".into(),
                size: big.len() as u64,
                folder: None,
                dir: None
            })
        );
        let mut got = Vec::new();
        for (i, expect_len) in [CHUNK_BYTES, CHUNK_BYTES, 10].into_iter().enumerate() {
            match &log[2 + i] {
                Sent::Chunk(c) => {
                    assert_eq!(c.len(), expect_len);
                    got.extend_from_slice(c);
                }
                other => panic!("expected chunk, got {other:?}"),
            }
        }
        assert_eq!(got, big);
        assert_eq!(log[5], msg(FileMessage::End { id: HOST_ID_BASE }));
        assert_eq!(
            log[6],
            msg(FileMessage::Start {
                id: HOST_ID_BASE + 1,
                name: "empty.txt".into(),
                size: 0,
                folder: None,
                dir: None
            })
        );
        assert_eq!(
            log[7],
            msg(FileMessage::End {
                id: HOST_ID_BASE + 1
            })
        );
    }

    #[tokio::test]
    async fn stale_list_id_is_ignored() {
        let mut h = Harness::new(true);
        let a = put(&h, "a.txt", b"x");
        h.command(FilesCommand::Offer(vec![a.clone()])).await;
        h.command(FilesCommand::Offer(vec![a])).await; // list 2
        h.feed(vec![fetch(1), fetch(99)]).await;
        assert!(h.session.downloader.sending.is_none());
        assert_eq!(h.sent().len(), 2);
    }

    #[tokio::test]
    async fn client_abort_stops_current_file_and_rest_of_list() {
        let mut h = Harness::new(true);
        let a = put(&h, "a.bin", &[1u8; 1000]);
        let b = put(&h, "b.bin", &[2u8; 1000]);
        h.command(FilesCommand::Offer(vec![a, b])).await;
        h.set_outstanding(SEND_HIGH_WATER + 1); // stall before the first chunk
        h.feed(vec![fetch(1)]).await;
        h.wait_log_len(2).await; // offer + start
        h.feed(vec![text(&FileMessage::Abort {
            id: HOST_ID_BASE,
            reason: AbortReason::Cancelled,
        })])
        .await;
        h.set_outstanding(0);
        h.wait_download_done().await;
        let log = h.log();
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(matches!(&log[1], Sent::Msg(FileMessage::Start { .. })));
    }

    #[tokio::test]
    async fn file_vanished_after_offer_is_aborted_and_next_one_goes() {
        let mut h = Harness::new(true);
        let a = put(&h, "a.txt", b"aaa");
        let b = put(&h, "b.txt", b"bb");
        h.command(FilesCommand::Offer(vec![a.clone(), b])).await;
        std::fs::remove_file(&a).unwrap();
        h.feed(vec![fetch(1)]).await;
        h.wait_download_done().await;
        let sent = h.sent();
        assert_eq!(
            sent[1..],
            [
                FileMessage::Start {
                    id: HOST_ID_BASE,
                    name: "a.txt".into(),
                    size: 3,
                    folder: None,
                    dir: None
                },
                FileMessage::Abort {
                    id: HOST_ID_BASE,
                    reason: AbortReason::Io
                },
                FileMessage::Start {
                    id: HOST_ID_BASE + 1,
                    name: "b.txt".into(),
                    size: 2,
                    folder: None,
                    dir: None
                },
                FileMessage::End {
                    id: HOST_ID_BASE + 1
                },
            ]
        );
    }

    #[tokio::test]
    async fn upload_during_download_goes_through() {
        let mut h = Harness::new(true);
        let a = put(&h, "down.bin", &[7u8; 100]);
        h.command(FilesCommand::Offer(vec![a])).await;
        h.set_outstanding(SEND_HIGH_WATER + 1);
        h.feed(vec![fetch(1)]).await;
        h.wait_log_len(2).await;
        // The download is stalled; an upload must still complete.
        h.feed(vec![
            start(1, "up.txt", 2),
            chunk(b"hi"),
            text(&FileMessage::End { id: 1 }),
        ])
        .await;
        assert!(h.sent().contains(&FileMessage::Saved {
            id: 1,
            name: "up.txt".into()
        }));
        assert_eq!(std::fs::read(h.dir.join("up.txt")).unwrap(), b"hi");
        assert!(h.session.downloader.sending.as_ref().unwrap().is_running());
        h.set_outstanding(0);
        h.wait_download_done().await;
        assert!(h.sent().contains(&FileMessage::End { id: HOST_ID_BASE }));
    }

    #[tokio::test]
    async fn fetch_during_download_is_ignored() {
        let mut h = Harness::new(true);
        let a = put(&h, "a.bin", &[1u8; 10]);
        h.command(FilesCommand::Offer(vec![a])).await;
        h.set_outstanding(SEND_HIGH_WATER + 1);
        h.feed(vec![fetch(1)]).await;
        h.wait_log_len(2).await;
        h.feed(vec![fetch(1)]).await;
        h.set_outstanding(0);
        h.wait_download_done().await;
        let starts = h
            .sent()
            .iter()
            .filter(|m| matches!(m, FileMessage::Start { .. }))
            .count();
        assert_eq!(starts, 1);
    }

    #[tokio::test]
    async fn run_serves_commands_and_channel_together() {
        let h = Harness::new(true);
        let a = put(&h, "a.txt", b"abc");
        let (tx, rx) = mpsc::channel(8);
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let task = tokio::spawn(run(
            Arc::clone(&h.fake) as Arc<dyn FileChannel>,
            rx,
            Some(h.dir.join("recv")),
            cmd_rx,
        ));
        cmd_tx.send(FilesCommand::Offer(vec![a])).await.unwrap();
        h.wait_log_len(1).await;
        tx.send(fetch(1)).await.unwrap();
        h.wait_log_len(4).await;
        assert_eq!(h.log()[2], Sent::Chunk(b"abc".to_vec()));
        drop(tx);
        task.await.unwrap();
    }
}
