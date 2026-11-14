//! The shared conversation model, holding images as GPUI images.

use std::sync::Arc;

use gpui::Image;
use loom_client::proto::SessionInfo;

pub use loom_view_model::conversation::*;

pub type Item = loom_view_model::conversation::Item<Arc<Image>>;
pub type Conversation = loom_view_model::conversation::Conversation<Arc<Image>>;

/// A conversation that decodes its images for GPUI.
pub fn new(info: SessionInfo) -> Conversation {
    Conversation::with_images(info, crate::attachments::decode)
}
