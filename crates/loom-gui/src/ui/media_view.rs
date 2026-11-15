//! Images and PDF files of the workspace, shown in their file's tab.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc;

use futures::StreamExt;
use futures::channel::mpsc as async_mpsc;
use gpui::{
    AnyElement, AsyncWindowContext, Bounds, Context, Image, ImageFormat, InteractiveElement,
    IntoElement, ListAlignment, ListState, ObjectFit, ParentElement, Pixels, Render, RenderImage,
    StatefulInteractiveElement, Styled, StyledImage, WeakEntity, Window, canvas, div, img, list,
    prelude::FluentBuilder, px, white,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};

/// The largest file the viewer opens.
pub const MAX_MEDIA_BYTES: u64 = 64 * 1024 * 1024;

/// Zoom levels the zoom buttons step through.
const ZOOMS: &[f32] = &[
    0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1., 1.1, 1.25, 1.5, 1.75, 2., 2.5, 3., 4., 6., 8.,
];

/// Logical pixels per PDF point at 100%: a point is 1/72 inch, and a logical
/// pixel 1/96.
const PIXELS_PER_POINT: f32 = 96. / 72.;

/// Room around and between the pages of a PDF file.
const PAGE_GAP: f32 = 12.;

/// The most pages kept rendered; the ones farthest from the view go first.
const KEPT_PAGES: usize = 12;

/// The longest side of a rendered page, in device pixels. Larger zooms
/// scale the bitmap up instead of rendering more pixels.
const MAX_RENDER_SIDE: f32 = 8192.;

/// What a file that the viewer opens holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image(ImageFormat),
    Pdf,
}

impl MediaKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Image(ImageFormat::Png) => "PNG image",
            Self::Image(ImageFormat::Jpeg) => "JPEG image",
            Self::Image(ImageFormat::Webp) => "WebP image",
            Self::Image(ImageFormat::Gif) => "GIF image",
            Self::Image(ImageFormat::Bmp) => "BMP image",
            Self::Image(ImageFormat::Tiff) => "TIFF image",
            Self::Image(ImageFormat::Ico) => "icon",
            Self::Image(_) => "image",
            Self::Pdf => "PDF",
        }
    }
}

/// The kind of media a workspace path holds, by its extension; `None` for
/// files the text view shows. SVG files are text and stay there.
pub fn media_kind(path: &str) -> Option<MediaKind> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (_, extension) = name.rsplit_once('.')?;
    let format = match extension.to_ascii_lowercase().as_str() {
        "pdf" => return Some(MediaKind::Pdf),
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "webp" => ImageFormat::Webp,
        "gif" => ImageFormat::Gif,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        _ => return None,
    };
    Some(MediaKind::Image(format))
}

/// How large the content is shown.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Zoom {
    /// As large as fits: images no larger than their own size, and PDF pages
    /// as wide as the view.
    Fit,
    Scale(f32),
}

/// An image's size in pixels as it is shown, after its EXIF orientation.
fn image_size(image: &Image) -> Option<(u32, u32)> {
    let format = match image.format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Webp => image::ImageFormat::WebP,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        ImageFormat::Ico => image::ImageFormat::Ico,
        _ => return None,
    };
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::with_format(Cursor::new(&image.bytes), format)
        .into_decoder()
        .ok()?;
    let (width, height) = decoder.dimensions();
    let turned = matches!(
        decoder.orientation(),
        Ok(image::metadata::Orientation::Rotate90
            | image::metadata::Orientation::Rotate270
            | image::metadata::Orientation::Rotate90FlipH
            | image::metadata::Orientation::Rotate270FlipH)
    );
    let size = if turned {
        (height, width)
    } else {
        (width, height)
    };
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// A page of a PDF file asked of the renderer thread, at `scale` device
/// pixels per point.
struct PageRequest {
    page: usize,
    scale: f32,
}

/// What the renderer thread sends back.
enum Rendered {
    /// The file was read: the size of each page, in points.
    Opened(Vec<(f32, f32)>),
    Failed(String),
    Page {
        page: usize,
        scale: f32,
        image: Arc<RenderImage>,
    },
}

/// Reads a PDF file and renders the pages it is asked for, in order, on a
/// thread of its own, until the view that started it is gone.
fn spawn_renderer(
    bytes: Vec<u8>,
    requests: mpsc::Receiver<PageRequest>,
    results: async_mpsc::UnboundedSender<Rendered>,
) {
    let spawned = std::thread::Builder::new()
        .name("pdf-renderer".into())
        .spawn(move || {
            use hayro::hayro_interpret::InterpreterSettings;
            use hayro::hayro_syntax::Pdf;
            use hayro::{PixmapSettings, RenderCache, RenderSettings};

            let pdf = match Pdf::new(bytes) {
                Ok(pdf) => pdf,
                Err(err) => {
                    let message = match err {
                        hayro::hayro_syntax::LoadPdfError::Decryption(_) => {
                            "the file is encrypted".to_string()
                        }
                        hayro::hayro_syntax::LoadPdfError::Invalid => {
                            "the file is not a valid PDF file".to_string()
                        }
                    };
                    let _ = results.unbounded_send(Rendered::Failed(message));
                    return;
                }
            };
            let pages = pdf.pages();
            let sizes = pages.iter().map(|page| page.render_dimensions()).collect();
            if results.unbounded_send(Rendered::Opened(sizes)).is_err() {
                return;
            }
            let cache = RenderCache::new();
            let interpreter = InterpreterSettings::default();
            let settings = RenderSettings::default();
            while let Ok(first) = requests.recv() {
                // Of the requests that piled up, only the latest for each
                // page counts.
                let mut batch = vec![first];
                batch.extend(requests.try_iter());
                let mut seen = HashSet::new();
                let mut latest: Vec<PageRequest> = batch
                    .into_iter()
                    .rev()
                    .filter(|request| seen.insert(request.page))
                    .collect();
                latest.reverse();
                for PageRequest { page, scale } in latest {
                    let Some(source) = pages.get(page) else {
                        continue;
                    };
                    let pixmap = hayro::render(
                        source,
                        &cache,
                        &interpreter,
                        &settings,
                        &PixmapSettings {
                            x_scale: scale,
                            y_scale: scale,
                            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
                        },
                    );
                    let (width, height) = (u32::from(pixmap.width()), u32::from(pixmap.height()));
                    let mut data = pixmap.data_as_u8_slice().to_vec();
                    // GPUI takes BGRA; the page is opaque, so premultiplied
                    // and straight alpha are the same.
                    for pixel in data.as_chunks_mut::<4>().0 {
                        pixel.swap(0, 2);
                    }
                    let Some(buffer) = image::RgbaImage::from_raw(width, height, data) else {
                        continue;
                    };
                    let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
                    if results
                        .unbounded_send(Rendered::Page { page, scale, image })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    if let Err(err) = spawned {
        tracing::warn!(error = %err, "could not start the PDF renderer");
    }
}

/// A page rendered at some scale.
struct PageImage {
    scale: f32,
    image: Arc<RenderImage>,
}

struct PdfDocument {
    /// Each page's size in points; empty until the file was read.
    pages: Vec<(f32, f32)>,
    requests: mpsc::Sender<PageRequest>,
    rendered: HashMap<usize, PageImage>,
    /// Pages asked for and the scale they were asked at.
    pending: HashMap<usize, f32>,
    /// The pages asked for, the latest last.
    recent: VecDeque<usize>,
    list: ListState,
}

enum Content {
    Image {
        image: Arc<Image>,
        size: Option<(u32, u32)>,
    },
    Pdf(PdfDocument),
    Failed(String),
}

pub struct MediaView {
    kind: MediaKind,
    content: Content,
    zoom: Zoom,
    /// The size of the area the content is shown in, as last painted.
    viewport: Option<Bounds<Pixels>>,
}

impl MediaView {
    pub fn new(
        kind: MediaKind,
        bytes: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let content = match kind {
            MediaKind::Image(format) => {
                let image = Arc::new(Image::from_bytes(format, bytes));
                let size = image_size(&image);
                match size {
                    Some(_) => Content::Image { image, size },
                    None => Content::Failed(format!(
                        "The file is not a {} that can be shown.",
                        kind.name()
                    )),
                }
            }
            MediaKind::Pdf => Content::Pdf(Self::open_pdf(bytes, window, cx)),
        };
        Self {
            kind,
            content,
            zoom: Zoom::Fit,
            viewport: None,
        }
    }

    fn open_pdf(bytes: Vec<u8>, window: &mut Window, cx: &mut Context<Self>) -> PdfDocument {
        let (requests, receiver) = mpsc::channel();
        let (sender, mut results) = async_mpsc::unbounded();
        spawn_renderer(bytes, receiver, sender);
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            while let Some(result) = results.next().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    this.received(result, window);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        PdfDocument {
            pages: Vec::new(),
            requests,
            rendered: HashMap::new(),
            pending: HashMap::new(),
            recent: VecDeque::new(),
            list: ListState::new(0, ListAlignment::Top, px(1000.)),
        }
    }

    fn received(&mut self, result: Rendered, window: &mut Window) {
        let Content::Pdf(pdf) = &mut self.content else {
            return;
        };
        match result {
            Rendered::Opened(pages) => {
                pdf.list.reset(pages.len());
                pdf.pages = pages;
            }
            Rendered::Failed(message) => {
                self.content = Content::Failed(format!("Could not open the PDF file: {message}."));
            }
            Rendered::Page { page, scale, image } => {
                if pdf.pending.get(&page) == Some(&scale) {
                    pdf.pending.remove(&page);
                }
                if let Some(old) = pdf.rendered.insert(page, PageImage { scale, image }) {
                    let _ = window.drop_image(old.image);
                }
                // Forgets the pages asked for longest ago.
                while pdf.rendered.len() > KEPT_PAGES {
                    let Some(oldest) = pdf.recent.pop_front() else {
                        break;
                    };
                    if pdf.recent.contains(&oldest) {
                        continue;
                    }
                    if let Some(old) = pdf.rendered.remove(&oldest) {
                        let _ = window.drop_image(old.image);
                    }
                }
            }
        }
    }

    /// Whether the file was read and can be shown.
    pub fn is_ready(&self) -> bool {
        match &self.content {
            Content::Image { .. } | Content::Failed(_) => true,
            Content::Pdf(pdf) => !pdf.pages.is_empty(),
        }
    }

    /// Why the file cannot be shown, if it cannot.
    pub fn error(&self) -> Option<&str> {
        match &self.content {
            Content::Failed(message) => Some(message),
            _ => None,
        }
    }

    pub fn page_count(&self) -> Option<usize> {
        match &self.content {
            Content::Pdf(pdf) => Some(pdf.pages.len()),
            _ => None,
        }
    }

    /// The pages rendered now, for tests.
    #[cfg(test)]
    pub(crate) fn rendered_pages(&self) -> Vec<usize> {
        match &self.content {
            Content::Pdf(pdf) => {
                let mut pages: Vec<usize> = pdf.rendered.keys().copied().collect();
                pages.sort_unstable();
                pages
            }
            _ => Vec::new(),
        }
    }

    /// The zoom that `Fit` stands for now, once the view was painted.
    fn fit_scale(&self) -> Option<f32> {
        let viewport = self.viewport?;
        match &self.content {
            Content::Image {
                size: Some((width, height)),
                ..
            } => Some(
                (f32::from(viewport.size.width) / *width as f32)
                    .min(f32::from(viewport.size.height) / *height as f32)
                    .min(1.),
            ),
            Content::Pdf(pdf) => {
                let widest = pdf.pages.iter().map(|(width, _)| *width).fold(0., f32::max);
                (widest > 0.).then(|| {
                    (f32::from(viewport.size.width) - 2. * PAGE_GAP).max(50.)
                        / (widest * PIXELS_PER_POINT)
                })
            }
            _ => None,
        }
    }

    fn scale(&self) -> f32 {
        match self.zoom {
            Zoom::Scale(scale) => scale,
            Zoom::Fit => self.fit_scale().unwrap_or(1.),
        }
    }

    fn zoom_by(&mut self, steps: i32, cx: &mut Context<Self>) {
        let current = self.scale();
        let next = if steps > 0 {
            ZOOMS.iter().copied().find(|zoom| *zoom > current * 1.01)
        } else {
            ZOOMS
                .iter()
                .rev()
                .copied()
                .find(|zoom| *zoom < current * 0.99)
        };
        if let Some(next) = next {
            self.set_zoom(Zoom::Scale(next), cx);
        }
    }

    fn set_zoom(&mut self, zoom: Zoom, cx: &mut Context<Self>) {
        if self.zoom == zoom {
            return;
        }
        self.zoom = zoom;
        if let Content::Pdf(pdf) = &self.content {
            pdf.list.remeasure();
        }
        cx.notify();
    }

    fn set_viewport(&mut self, viewport: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.viewport.map(|old| old.size) == Some(viewport.size) {
            self.viewport = Some(viewport);
            return;
        }
        self.viewport = Some(viewport);
        if self.zoom == Zoom::Fit {
            if let Content::Pdf(pdf) = &self.content {
                pdf.list.remeasure();
            }
            cx.notify();
        }
    }

    /// Asks for a page at `scale` device pixels per point unless it is
    /// rendered or asked for at about that scale.
    fn want_page(&mut self, page: usize, scale: f32) {
        let Content::Pdf(pdf) = &mut self.content else {
            return;
        };
        let close = |other: f32| (other / scale - 1.).abs() < 0.1;
        pdf.recent.retain(|recent| *recent != page);
        pdf.recent.push_back(page);
        if pdf.rendered.get(&page).is_some_and(|it| close(it.scale))
            || pdf.pending.get(&page).is_some_and(|it| close(*it))
        {
            return;
        }
        pdf.pending.insert(page, scale);
        let _ = pdf.requests.send(PageRequest { page, scale });
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let label = match self.zoom {
            Zoom::Fit => match self.fit_scale() {
                Some(scale) => format!("Fit ({:.0}%)", scale * 100.),
                None => "Fit".to_string(),
            },
            Zoom::Scale(scale) => format!("{:.0}%", scale * 100.),
        };
        let scale = self.scale();
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                Button::new("media-zoom-out")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Minus))
                    .tooltip("Zoom out")
                    .disabled(scale <= ZOOMS[0] * 1.01)
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_by(-1, cx))),
            )
            .child(
                div()
                    .id("media-zoom")
                    .debug_selector(|| "media-zoom".into())
                    .min_w(px(64.))
                    .text_center()
                    .child(label),
            )
            .child(
                Button::new("media-zoom-in")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Plus))
                    .tooltip("Zoom in")
                    .disabled(scale >= ZOOMS[ZOOMS.len() - 1] * 0.99)
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_by(1, cx))),
            )
            .child(
                Button::new("media-fit")
                    .ghost()
                    .xsmall()
                    .label(if matches!(self.kind, MediaKind::Pdf) {
                        "Fit width"
                    } else {
                        "Fit"
                    })
                    .disabled(self.zoom == Zoom::Fit)
                    .on_click(cx.listener(|this, _, _, cx| this.set_zoom(Zoom::Fit, cx))),
            )
            .child(
                Button::new("media-actual-size")
                    .ghost()
                    .xsmall()
                    .label("100%")
                    .disabled(self.zoom == Zoom::Scale(1.))
                    .on_click(cx.listener(|this, _, _, cx| this.set_zoom(Zoom::Scale(1.), cx))),
            )
            .child(div().flex_1())
            .when_some(self.details(), |el, details| el.child(details))
    }

    /// The image's size, or the PDF file's page count.
    fn details(&self) -> Option<String> {
        match &self.content {
            Content::Image {
                size: Some((width, height)),
                ..
            } => Some(format!("{width} × {height}")),
            Content::Pdf(pdf) if !pdf.pages.is_empty() => Some(match pdf.pages.len() {
                1 => "1 page".to_string(),
                count => format!("{count} pages"),
            }),
            _ => None,
        }
    }

    fn render_image(&self, image: &Arc<Image>, size: Option<(u32, u32)>) -> AnyElement {
        match (self.zoom, size) {
            (Zoom::Scale(scale), Some((width, height))) => div()
                .id("media-scroll")
                .size_full()
                .overflow_scroll()
                .child(
                    div()
                        .flex()
                        .min_w_full()
                        .min_h_full()
                        .items_center()
                        .justify_center()
                        .child(
                            img(image.clone())
                                .flex_none()
                                .w(px(width as f32 * scale))
                                .h(px(height as f32 * scale)),
                        ),
                )
                .into_any_element(),
            _ => div()
                .size_full()
                .p_2()
                .child(
                    img(image.clone())
                        .size_full()
                        .object_fit(ObjectFit::ScaleDown),
                )
                .into_any_element(),
        }
    }

    fn render_pdf(
        &self,
        pdf: &PdfDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if pdf.pages.is_empty() {
            return div().p_4().child("Opening…").into_any_element();
        }
        let zoom = self.scale() * PIXELS_PER_POINT;
        let device = (zoom * window.scale_factor()).max(0.05);
        let this: WeakEntity<Self> = cx.entity().downgrade();
        let pages = pdf.pages.clone();
        let images: HashMap<usize, Arc<RenderImage>> = pdf
            .rendered
            .iter()
            .map(|(page, rendered)| (*page, rendered.image.clone()))
            .collect();
        let backdrop = cx.theme().muted;
        list(pdf.list.clone(), move |ix, _, cx| {
            let (width, height) = pages[ix];
            let scale = device.min(MAX_RENDER_SIDE / width.max(height).max(1.));
            let _ = this.update(cx, |this, _| this.want_page(ix, scale));
            let page = div()
                .flex_none()
                .w(px(width * zoom))
                .h(px(height * zoom))
                .bg(white())
                .shadow_sm();
            let page = match images.get(&ix) {
                Some(image) => page.child(img(image.clone()).size_full()),
                None => page,
            };
            div()
                .w_full()
                .flex()
                .justify_center()
                .pt(px(PAGE_GAP))
                .when(ix + 1 == pages.len(), |el| el.pb(px(PAGE_GAP)))
                .child(page)
                .into_any_element()
        })
        .size_full()
        .bg(backdrop)
        .into_any_element()
    }
}

impl Render for MediaView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.content {
            Content::Failed(message) => div()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child(message.clone())
                .into_any_element(),
            Content::Image { image, size } => self.render_image(image, *size),
            Content::Pdf(pdf) => self.render_pdf(pdf, window, cx),
        };
        let this = cx.entity();
        v_flex()
            .size_full()
            .when(!matches!(self.content, Content::Failed(_)), |el| {
                el.child(self.toolbar(cx))
            })
            .child(
                div()
                    .id("media-body")
                    .debug_selector(|| "media-body".into())
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                this.update(cx, |view, cx| view.set_viewport(bounds, cx));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_is_told_by_the_extension() {
        assert_eq!(
            media_kind("docs/shot.PNG"),
            Some(MediaKind::Image(ImageFormat::Png))
        );
        assert_eq!(
            media_kind("a.jpeg"),
            Some(MediaKind::Image(ImageFormat::Jpeg))
        );
        assert_eq!(media_kind("paper.pdf"), Some(MediaKind::Pdf));
        assert_eq!(media_kind("logo.svg"), None);
        assert_eq!(media_kind("pdf"), None);
        assert_eq!(media_kind("dir.png/file"), None);
        assert_eq!(media_kind("src/main.rs"), None);
    }

    #[test]
    fn image_sizes_come_from_the_header() {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::new(30, 20)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let png = Image::from_bytes(ImageFormat::Png, bytes.into_inner());
        assert_eq!(image_size(&png), Some((30, 20)));
        let broken = Image::from_bytes(ImageFormat::Png, b"not a png".to_vec());
        assert_eq!(image_size(&broken), None);
    }
}
