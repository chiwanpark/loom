use std::sync::Arc;

use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, Image, ImageFormat, InteractiveElement,
    IntoElement, ObjectFit, ParentElement, Pixels, Render, StatefulInteractiveElement, Styled,
    StyledImage, Window, div, img, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex};

fn format_name(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Webp => "WebP",
        ImageFormat::Gif => "GIF",
        ImageFormat::Svg => "SVG",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Tiff => "TIFF",
        _ => "Image",
    }
}

fn size_label(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.0} KB", (bytes as f64 / 1024.).ceil())
    }
}

struct PreviewBody {
    image: Arc<Image>,
    height: Pixels,
    fit: bool,
}

impl Render for PreviewBody {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fit = if self.fit {
            ObjectFit::Contain
        } else {
            ObjectFit::ScaleDown
        };
        div()
            .id("image-preview")
            .w_full()
            .h(self.height)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.fit = !this.fit;
                cx.notify();
            }))
            .child(img(self.image.clone()).size_full().object_fit(fit))
    }
}

pub fn open_image_preview(image: Arc<Image>, window: &mut Window, cx: &mut App) {
    let viewport = window.viewport_size();
    let width = (viewport.width * 0.9).min(px(1800.));
    let height = (viewport.height * 0.9 - px(140.)).max(px(160.));
    let title = format!(
        "{} · {}",
        format_name(image.format),
        size_label(image.bytes.len())
    );
    let body: Entity<PreviewBody> = cx.new(|_| PreviewBody {
        image: image.clone(),
        height,
        fit: false,
    });
    window.open_dialog(cx, move |dialog, _, cx| {
        let copy = image.clone();
        let muted = cx.theme().muted_foreground;
        dialog
            .w(width)
            .margin_top(viewport.height * 0.04)
            .overlay_closable(true)
            .title(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().text_color(muted).child(title.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("click the image to toggle actual size and fit to window"),
                    )
                    .child(
                        Button::new("copy-image")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Copy))
                            .tooltip("Copy the image")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_image(&copy))
                            }),
                    ),
            )
            .child(body.clone())
    });
}
