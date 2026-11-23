use std::sync::Mutex;

use crate::error::LlmError;

pub(super) const CLAUDE_CODE_VERSION: &str = "2.1.323";
pub(super) const REJECTION: &str = "claude_code_version_too_old";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version(u32, u32, u32);

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let mut next = || {
            let part = parts.next()?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        };
        let version = Self(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    }

    pub fn required(message: &str) -> Option<Self> {
        let (prefix, _) = message.split_once(" or newer is required")?;
        let (_, version) = prefix.rsplit_once("version ")?;
        Self::parse(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

pub(super) struct Floor(Mutex<Version>);

impl Default for Floor {
    fn default() -> Self {
        Self(Mutex::new(Version::parse(CLAUDE_CODE_VERSION).unwrap()))
    }
}

impl Floor {
    pub fn get(&self) -> Version {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn learn(&self, version: Version) -> Version {
        let mut floor = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *floor = (*floor).max(version);
        *floor
    }
}

pub(super) fn add_hint(error: &mut LlmError, billed: bool, sent: Version) {
    if let LlmError::Api { message, .. } = error {
        if billed {
            message.push_str(&format!(
                " Loom sent billing version {sent}. Update Loom or select a different model."
            ));
        } else {
            message.push_str(" Loom could not build a billing block because the first user message has no text. Start a session with a text message or update Loom.");
        }
    }
}
