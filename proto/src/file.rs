//! Messages sent over the `file` data channel (slice 3.4, see
//! ARCHITECTURE.md §5): file transfer in both directions over one ordered,
//! reliable channel.
//!
//! The channel carries two kinds of messages. Text messages are JSON-encoded
//! `FileMessage`s. Binary messages are raw file chunks with no header, at
//! most 64 KiB each.
//!
//! Protocol rule: in each direction only one transfer is in flight at a
//! time. The channel is ordered, so a binary chunk always belongs to the
//! most recent `Start` sent by that side. A transfer is `Start`, any number
//! of binary chunks, then `End`. A receiver that itself sent `Abort` for the
//! current transfer silently drops chunks until the sender's `End`, `Abort`
//! or a new `Start` arrives (the sender may not have seen the `Abort` yet).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

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
    Start {
        id: u32,
        name: String,
        #[ts(type = "number")]
        size: u64,
    },
    /// Sender -> receiver: all chunks of transfer `id` have been sent.
    End { id: u32 },
    /// Either side: transfer `id` is aborted.
    Abort { id: u32, reason: AbortReason },
    /// Host -> client: the host saved the received transfer `id` under
    /// `name`, the final name after sanitizing and de-duplication.
    Saved { id: u32, name: String },
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
            },
            r#"{"type":"start","id":7,"name":"a.txt","size":9007199254740000}"#,
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
}
