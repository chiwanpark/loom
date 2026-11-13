pub mod code;
pub mod dialogs;
pub mod diff;
pub mod download;
pub mod file_view;
pub mod files;
pub mod image_preview;
pub mod messages;
pub mod notes_view;
pub mod notify;
pub mod panes;
pub mod picker;
pub mod session;
pub mod tab;
pub mod tabs;
pub mod tasks_view;
pub mod workspace;

pub use workspace::{Workspace, bind_keys};
