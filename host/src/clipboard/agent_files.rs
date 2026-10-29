//! D87d: file list handoff from the tray agent to the full-mode helper.
//!
//! In full mode on Windows the helper runs as SYSTEM and cannot read the
//! files Explorer put on the clipboard (only the OLE `DataObject` format is
//! visible to it, and `GetData(CF_HDROP)` fails with `DV_E_FORMATETC`). The
//! tray agent (`rcdesk-agent`, running as the user) can, so it watches the
//! clipboard change counter, reads the file list and writes it to
//! [`AGENT_FILES_FILE`] in the user's data directory together with the
//! counter value (`seq`). The helper reads the file for the current counter
//! value via [`wait_for_seq`]. A stale list is impossible: an entry is only
//! accepted when its `seq` equals the current clipboard counter.
//!
//! Security: the file is writable by any process of the user, and the SYSTEM
//! helper offers the paths found there to the client. That is the same trust
//! level as `CF_HDROP` on the shared clipboard (any process of the user puts
//! that there too), so this adds no new attack surface.
//!
//! The module is platform independent (pure logic), only its users are
//! Windows-specific.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Name of the handoff file inside the user's rcdesk data directory.
pub const AGENT_FILES_FILE: &str = "clipboard-files.json";

/// The file list the agent saw on the clipboard at counter value `seq`
/// (empty `paths`: no files there).
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct AgentFiles {
    pub seq: u64,
    pub paths: Vec<PathBuf>,
}

/// Writes `files` atomically (temporary file + rename) into `dir`, creating
/// the directory if needed.
pub fn write_agent_files(dir: &Path, files: &AgentFiles) -> io::Result<()> {
    let bytes = serde_json::to_vec(files).map_err(io::Error::other)?;
    std::fs::create_dir_all(dir)?;
    crate::fsutil::write_private_atomic(&dir.join(AGENT_FILES_FILE), &bytes)
}

/// Reads the handoff file; `None` when it is missing or not valid JSON.
pub fn read_agent_files(dir: &Path) -> Option<AgentFiles> {
    let bytes = std::fs::read(dir.join(AGENT_FILES_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Polls the handoff file every `step` for up to `timeout` until it holds an
/// entry with `seq`. A matching non-empty list is returned; a matching empty
/// list means "no files" (`None`); on timeout also `None`.
pub fn wait_for_seq(
    dir: &Path,
    seq: u64,
    timeout: Duration,
    step: Duration,
) -> Option<Vec<PathBuf>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(files) = read_agent_files(dir) {
            if files.seq == seq {
                return (!files.paths.is_empty()).then_some(files.paths);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rcdesk-agent-files-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample(seq: u64) -> AgentFiles {
        AgentFiles {
            seq,
            paths: vec![PathBuf::from("a.txt"), PathBuf::from("b/c.txt")],
        }
    }

    const SHORT: Duration = Duration::from_millis(50);
    const STEP: Duration = Duration::from_millis(10);

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tmp("roundtrip");
        write_agent_files(&dir, &sample(7)).expect("write");
        assert_eq!(read_agent_files(&dir), Some(sample(7)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_reads_none() {
        let dir = tmp("missing");
        assert_eq!(read_agent_files(&dir), None);
    }

    #[test]
    fn broken_file_reads_none() {
        let dir = tmp("broken");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(AGENT_FILES_FILE), b"{not json").expect("write");
        assert_eq!(read_agent_files(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_finds_matching_seq() {
        let dir = tmp("match");
        write_agent_files(&dir, &sample(5)).expect("write");
        assert_eq!(wait_for_seq(&dir, 5, SHORT, STEP), Some(sample(5).paths));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_rejects_other_seq() {
        let dir = tmp("other");
        write_agent_files(&dir, &sample(5)).expect("write");
        assert_eq!(wait_for_seq(&dir, 6, SHORT, STEP), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_sees_file_written_later() {
        let dir = tmp("later");
        let writer_dir = dir.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            write_agent_files(&writer_dir, &sample(9)).expect("write");
        });
        let got = wait_for_seq(&dir, 9, Duration::from_secs(2), STEP);
        writer.join().expect("writer thread");
        assert_eq!(got, Some(sample(9).paths));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_empty_list_is_none() {
        let dir = tmp("empty");
        let files = AgentFiles {
            seq: 3,
            paths: Vec::new(),
        };
        write_agent_files(&dir, &files).expect("write");
        assert_eq!(wait_for_seq(&dir, 3, SHORT, STEP), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
