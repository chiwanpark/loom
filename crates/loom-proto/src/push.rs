//! What a push notification tells a client. An agent encodes it with
//! [`crate::encode`] and seals it for the client (`loom_crypto::seal_push`);
//! the relay hands it to the client's push service.

use serde::{Deserialize, Serialize};

use crate::api::InstanceId;
use crate::session::SessionId;

/// The version of [`PushNotice`] agents send.
pub const PUSH_NOTICE_VERSION: u16 = 1;
/// The most characters of each text a notice carries; push services limit
/// the size of a message to a few kilobytes.
pub const MAX_PUSH_TEXT: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushKind {
    /// A tool call waits for the user's approval.
    Approval,
    /// A turn completed.
    Finished,
    /// A turn failed.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushNotice {
    pub version: u16,
    pub instance: InstanceId,
    pub workspace: String,
    pub session: SessionId,
    pub title: Option<String>,
    pub kind: PushKind,
    /// The tool call that waits, the start of the answer, or the error.
    pub detail: Option<String>,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

/// `text` cut to [`MAX_PUSH_TEXT`] characters, with an ellipsis when cut.
pub fn push_text(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_PUSH_TEXT) {
        Some((end, _)) => format!("{}…", text[..end].trim_end()),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    #[test]
    fn notices_round_trip_and_stay_small() {
        let notice = PushNotice {
            version: PUSH_NOTICE_VERSION,
            instance: InstanceId("i".repeat(32)),
            workspace: push_text(&"/w".repeat(400)),
            session: SessionId("s".repeat(32)),
            title: Some(push_text(&"t".repeat(400))),
            kind: PushKind::Approval,
            detail: Some(push_text(&"한".repeat(400))),
            at: 1,
        };
        let bytes = encode(&notice).unwrap();
        assert!(bytes.len() < 2048, "{} bytes", bytes.len());
        assert_eq!(decode::<PushNotice>(&bytes).unwrap(), notice);
    }

    #[test]
    fn long_texts_are_cut() {
        assert_eq!(push_text("  short  "), "short");
        let cut = push_text(&"한".repeat(MAX_PUSH_TEXT + 1));
        assert_eq!(cut.chars().count(), MAX_PUSH_TEXT + 1);
        assert!(cut.ends_with('…'));
    }
}
