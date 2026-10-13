//! Messages sent over the `file` data channel (slice 3.4, see
//! ARCHITECTURE.md §5): file transfer in both directions over one ordered,
//! reliable channel.
//!
//! The channel carries two kinds of messages. Text messages are JSON-encoded
//! `FileMessage`s. Binary messages are raw file chunks with no header, at
//! most 64 KiB each.
//!
//! Protocol rule: the two directions are independent. An upload (client ->
//! host) and a download (host -> client) may run at the same time over the
//! one ordered channel. In each direction only one transfer is in flight at
//! a time. A binary chunk always belongs to the most recent `Start` sent by
//! the SAME side that sent the chunk; this is unambiguous for each receiver
//! because it only ever gets chunks from the other side. A transfer is
//! `Start`, any number of binary chunks, then `End`. A receiver that itself
//! sent `Abort` for the current transfer silently drops chunks until the
//! sender's `End`, `Abort` or a new `Start` arrives (the sender may not have
//! seen the `Abort` yet).
//!
//! Transfer ids: the client numbers its uploads from 1, the host numbers its
//! downloads from `HOST_ID_BASE` (0x8000_0000), so an `Abort { id }` is
//! never ambiguous between the two directions.
//!
//! Download flow (slice 3.4c): when the host user copies files, the host
//! sends `Offer`; the client asks for them with `Fetch`; the host then sends
//! every file as `Start` / chunks / `End`. The client may `Abort` the current
//! file with `Cancelled`, which also drops the rest of the list. The host
//! may `Abort` a single file with `Io` (vanished, shorter than announced,
//! read error); the client discards it and waits for the next `Start`.
//! `Saved` is not used for downloads.
//!
//! Folder upload (slice 3.4e): the client sends a folder as a plain sequence
//! of single-file transfers. Each `Start` of such a file carries `folder`
//! (a number the client picks, unique per sent root folder within the
//! session) and `dir` (the file's directory relative to Downloads, `/`
//! separated, the root folder name first, e.g. `Photos/2024/may`). The host
//! creates the root folder once per `folder` (de-duplicating its name) and
//! puts every file of that `folder` under it. Both fields are sent together;
//! if only one arrives, the file is saved flat like an ordinary upload. Both
//! are omitted from the JSON when absent, so the old format stays valid.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// First transfer id used by the host for downloads (host -> client); the
/// client numbers its uploads from 1. See the module docs.
pub const HOST_ID_BASE: u32 = 0x8000_0000;

/// One file in a host -> client `Offer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct OfferedFile {
    pub name: String,
    #[ts(type = "number")]
    pub size: u64,
}

/// Why a transfer was aborted, carried in `FileMessage::Abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AbortReason {
    /// The user cancelled the transfer.
    Cancelled,
    /// File transfer is disabled on the host (`serve --no-files`).
    Disabled,
    /// A disk read/write error on the aborting side.
    Io,
    /// The peer broke the protocol (size mismatch, unexpected message, ...).
    Protocol,
}

/// A single text message on the `file` channel, JSON-encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum FileMessage {
    /// Sender -> receiver: a transfer begins. `id` identifies the transfer
    /// in later messages; `name` is the file name as the sender knows it
    /// (the receiver sanitizes it); `size` is the exact total byte count
    /// (always below 2^53, so it is a plain `number` in TypeScript).
    /// `folder` / `dir` (slice 3.4e, see the module docs) place the file
    /// inside an uploaded folder; `name` stays the bare file name.
    Start {
        id: u32,
        name: String,
        #[ts(type = "number")]
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<String>,
    },
    /// Sender -> receiver: all chunks of transfer `id` have been sent.
    End { id: u32 },
    /// Either side: transfer `id` is aborted.
    Abort { id: u32, reason: AbortReason },
    /// Host -> client: the host saved the received transfer `id` under
    /// `name`, the final name after sanitizing and de-duplication.
    Saved { id: u32, name: String },
    /// Host -> client: the host user copied files; they can be downloaded
    /// with `Fetch { list_id }`. Folders are not supported and only counted
    /// in `skipped_folders`. An empty `files` with `skipped_folders == 0`
    /// means "no offer any more".
    Offer {
        list_id: u32,
        files: Vec<OfferedFile>,
        skipped_folders: u32,
    },
    /// Client -> host: send all files of offer `list_id`.
    Fetch { list_id: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(msg: FileMessage, expected: &str) {
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, expected);
        let round_tripped: FileMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn start_has_exact_shape() {
        check(
            FileMessage::Start {
                id: 7,
                name: "a.txt".to_string(),
                size: 9_007_199_254_740_000,
                folder: None,
                dir: None,
            },
            r#"{"type":"start","id":7,"name":"a.txt","size":9007199254740000}"#,
        );
    }

    #[test]
    fn start_with_folder_has_exact_shape() {
        check(
            FileMessage::Start {
                id: 8,
                name: "b.txt".to_string(),
                size: 5,
                folder: Some(2),
                dir: Some("Фото/2024".to_string()),
            },
            r#"{"type":"start","id":8,"name":"b.txt","size":5,"folder":2,"dir":"Фото/2024"}"#,
        );
    }

    #[test]
    fn start_without_folder_fields_deserializes_to_none() {
        let msg: FileMessage =
            serde_json::from_str(r#"{"type":"start","id":1,"name":"a","size":0}"#).unwrap();
        assert_eq!(
            msg,
            FileMessage::Start {
                id: 1,
                name: "a".into(),
                size: 0,
                folder: None,
                dir: None
            }
        );
    }

    #[test]
    fn end_has_exact_shape() {
        check(FileMessage::End { id: 3 }, r#"{"type":"end","id":3}"#);
    }

    #[test]
    fn abort_has_exact_shape_for_every_reason() {
        for (reason, wire) in [
            (AbortReason::Cancelled, "cancelled"),
            (AbortReason::Disabled, "disabled"),
            (AbortReason::Io, "io"),
            (AbortReason::Protocol, "protocol"),
        ] {
            check(
                FileMessage::Abort { id: 1, reason },
                &format!(r#"{{"type":"abort","id":1,"reason":"{wire}"}}"#),
            );
        }
    }

    #[test]
    fn saved_has_exact_shape() {
        check(
            FileMessage::Saved {
                id: 2,
                name: "a (1).txt".to_string(),
            },
            r#"{"type":"saved","id":2,"name":"a (1).txt"}"#,
        );
    }

    #[test]
    fn offer_has_exact_shape() {
        check(
            FileMessage::Offer {
                list_id: 4,
                files: vec![
                    OfferedFile {
                        name: "a.txt".to_string(),
                        size: 12,
                    },
                    OfferedFile {
                        name: "b.bin".to_string(),
                        size: 0,
                    },
                ],
                skipped_folders: 2,
            },
            r#"{"type":"offer","list_id":4,"files":[{"name":"a.txt","size":12},{"name":"b.bin","size":0}],"skipped_folders":2}"#,
        );
        check(
            FileMessage::Offer {
                list_id: 5,
                files: vec![],
                skipped_folders: 0,
            },
            r#"{"type":"offer","list_id":5,"files":[],"skipped_folders":0}"#,
        );
    }

    #[test]
    fn fetch_has_exact_shape() {
        check(
            FileMessage::Fetch { list_id: 4 },
            r#"{"type":"fetch","list_id":4}"#,
        );
    }
}
