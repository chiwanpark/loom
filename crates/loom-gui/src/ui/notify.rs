use gpui::{App, Window};
use gpui_component::WindowExt;
use gpui_component::notification::Notification;

pub fn error(window: &mut Window, cx: &mut App, message: impl Into<String>) {
    let message = message.into();
    tracing::warn!("{message}");
    window.push_notification(Notification::error(message), cx);
}

pub fn info(window: &mut Window, cx: &mut App, message: impl Into<String>) {
    window.push_notification(Notification::info(message.into()), cx);
}

pub fn success(window: &mut Window, cx: &mut App, message: impl Into<String>) {
    window.push_notification(Notification::success(message.into()), cx);
}
