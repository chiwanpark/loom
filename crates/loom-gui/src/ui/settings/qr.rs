use gpui::{
    Bounds, InteractiveElement, IntoElement, ParentElement, Pixels, Styled, canvas, div, fill,
    point, px, rgb, size,
};
use qrcode::{Color, QrCode};

pub fn qr_code(data: &str, side: f32) -> Option<impl IntoElement> {
    let code = QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let quiet = 4;
    let cells = width + 2 * quiet;
    Some(
        div()
            .size(px(side))
            .bg(rgb(0xffffff))
            .debug_selector(|| "settings-qr".into())
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds: Bounds<Pixels>, _, window, _| {
                        let cell = bounds.size.width.min(bounds.size.height) / cells as f32;
                        for (ix, color) in colors.iter().enumerate() {
                            if *color != Color::Dark {
                                continue;
                            }
                            let (x, y) = (ix % width + quiet, ix / width + quiet);
                            let origin = point(
                                bounds.origin.x + cell * x as f32,
                                bounds.origin.y + cell * y as f32,
                            );
                            window.paint_quad(fill(
                                Bounds::new(origin, size(cell, cell)),
                                rgb(0x000000),
                            ));
                        }
                    },
                )
                .size_full(),
            ),
    )
}
