mod codex;
mod openai;

use crate::kind::ProviderKind;

pub(crate) static KINDS: &[&dyn ProviderKind] = &[&openai::Kind, &codex::Kind];
