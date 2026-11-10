mod codex;
pub(crate) mod compatible;
mod openai;

use std::sync::Arc;

use crate::kind::ProviderKind;

pub(crate) fn builtin() -> Vec<Arc<dyn ProviderKind>> {
    vec![Arc::new(openai::Kind), Arc::new(codex::Kind)]
}
