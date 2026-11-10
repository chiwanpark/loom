use std::path::Path;
use std::sync::Arc;

use gpui::{Image, ImageFormat};

pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_PROMPT_IMAGE_BYTES: usize = 12 * 1024 * 1024;

const SUPPORTED: [ImageFormat; 4] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::Webp,
    ImageFormat::Gif,
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AttachError {
    #[error("{0}: only PNG, JPEG, WebP, and GIF images can be attached")]
    Unsupported(String),
    #[error("{name} is {size} bytes, larger than the {MAX_IMAGE_BYTES} byte limit")]
    TooLarge { name: String, size: usize },
    #[error("attached images exceed {MAX_PROMPT_IMAGE_BYTES} bytes in total")]
    TotalTooLarge,
    #[error("{0}: {1}")]
    Read(String, String),
}

pub fn is_supported(format: ImageFormat) -> bool {
    SUPPORTED.contains(&format)
}

pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

pub fn from_bytes(name: &str, bytes: Vec<u8>) -> Result<Arc<Image>, AttachError> {
    let format = sniff(&bytes).ok_or_else(|| AttachError::Unsupported(name.to_owned()))?;
    check_size(name, bytes.len())?;
    Ok(Arc::new(Image::from_bytes(format, bytes)))
}

pub fn from_image(image: &Image) -> Result<Arc<Image>, AttachError> {
    if !is_supported(image.format) {
        return Err(AttachError::Unsupported("pasted image".into()));
    }
    check_size("pasted image", image.bytes.len())?;
    Ok(Arc::new(image.clone()))
}

pub fn from_path(path: &Path) -> Result<Arc<Image>, AttachError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let size = std::fs::metadata(path)
        .map_err(|e| AttachError::Read(name.clone(), e.to_string()))?
        .len() as usize;
    check_size(&name, size)?;
    let bytes = std::fs::read(path).map_err(|e| AttachError::Read(name.clone(), e.to_string()))?;
    from_bytes(&name, bytes)
}

pub fn check_total(existing: &[Arc<Image>], added: &Image) -> Result<(), AttachError> {
    let total: usize = existing.iter().map(|i| i.bytes.len()).sum::<usize>() + added.bytes.len();
    if total > MAX_PROMPT_IMAGE_BYTES {
        Err(AttachError::TotalTooLarge)
    } else {
        Ok(())
    }
}

fn check_size(name: &str, size: usize) -> Result<(), AttachError> {
    if size > MAX_IMAGE_BYTES {
        Err(AttachError::TooLarge {
            name: name.to_owned(),
            size,
        })
    } else {
        Ok(())
    }
}

pub fn decode(mime_type: &str, data: &[u8]) -> Option<Arc<Image>> {
    let format = ImageFormat::from_mime_type(mime_type).or_else(|| sniff(data))?;
    Some(Arc::new(Image::from_bytes(format, data.to_vec())))
}

/// The image's size in pixels, read from its header.
pub fn dimensions(image: &Image) -> Option<(u32, u32)> {
    let format = match image.format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Webp => image::ImageFormat::WebP,
        ImageFormat::Gif => image::ImageFormat::Gif,
        _ => return None,
    };
    image::ImageReader::with_format(std::io::Cursor::new(&image.bytes), format)
        .into_dimensions()
        .ok()
        .filter(|&(width, height)| width > 0 && height > 0)
}

/// Scales `size` down to fit in `max`, keeping its aspect ratio. Smaller
/// images keep their size.
pub fn fit_within(size: (u32, u32), max: (f32, f32)) -> (f32, f32) {
    let (width, height) = (size.0 as f32, size.1 as f32);
    let scale = (max.0 / width).min(max.1 / height).min(1.);
    (width * scale, height * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Image {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(width, height)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        Image::from_bytes(ImageFormat::Png, bytes.into_inner())
    }

    #[test]
    fn reads_dimensions_from_the_header() {
        assert_eq!(dimensions(&png(300, 40)), Some((300, 40)));
        assert_eq!(
            dimensions(&Image::from_bytes(ImageFormat::Png, b"nope".to_vec())),
            None
        );
    }

    #[test]
    fn fits_keeping_the_aspect_ratio() {
        // A wide screenshot is limited by the width, not stretched to the
        // height limit.
        let (w, h) = fit_within((1850, 595), (480., 320.));
        assert_eq!(w, 480.);
        assert!((h - 154.378).abs() < 0.01, "{h}");
        assert_eq!(fit_within((400, 1600), (480., 320.)), (80., 320.));
        assert_eq!(fit_within((100, 50), (480., 320.)), (100., 50.));
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn sniffs_supported_formats() {
        assert_eq!(sniff(PNG), Some(ImageFormat::Png));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some(ImageFormat::Jpeg));
        assert_eq!(sniff(b"GIF89a..."), Some(ImageFormat::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(ImageFormat::Webp));
        assert_eq!(sniff(b"<svg"), None);
    }

    #[test]
    fn rejects_unsupported_and_oversized_images() {
        assert_eq!(
            from_bytes("a.txt", b"hello".to_vec()).unwrap_err(),
            AttachError::Unsupported("a.txt".into())
        );
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(matches!(
            from_bytes("big.png", big),
            Err(AttachError::TooLarge { .. })
        ));
        let image = from_bytes("a.png", PNG.to_vec()).unwrap();
        assert_eq!(image.format, ImageFormat::Png);
        let svg = Image::from_bytes(ImageFormat::Svg, b"<svg/>".to_vec());
        assert!(from_image(&svg).is_err());
    }

    #[test]
    fn limits_the_total_size() {
        let mut chunk = PNG.to_vec();
        chunk.resize(MAX_IMAGE_BYTES, 0);
        let one = from_bytes("a.png", chunk.clone()).unwrap();
        let two = from_bytes("b.png", chunk.clone()).unwrap();
        let three = from_bytes("c.png", chunk).unwrap();
        assert!(check_total(std::slice::from_ref(&one), &two).is_ok());
        assert_eq!(
            check_total(&[one, two], &three),
            Err(AttachError::TotalTooLarge)
        );
    }

    #[test]
    fn decodes_events_by_mime_type() {
        let image = decode("image/png", PNG).unwrap();
        assert_eq!(image.format, ImageFormat::Png);
        assert!(decode("application/octet-stream", b"nope").is_none());
    }
}
