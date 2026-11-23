mod anthropic;
pub(crate) mod codex;
pub(crate) mod compatible;
mod copilot;
mod openai;
mod opencode_go;

use std::sync::Arc;

use crate::kind::ProviderKind;

pub(crate) fn builtin() -> Vec<Arc<dyn ProviderKind>> {
    vec![
        Arc::new(anthropic::Kind),
        Arc::new(copilot::Kind),
        Arc::new(openai::Kind),
        Arc::new(codex::Kind),
        Arc::new(opencode_go::Kind),
    ]
}
