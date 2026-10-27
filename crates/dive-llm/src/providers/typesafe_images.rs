use crate::image::ImageData;
use crate::LlmError;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{
    DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Rgba, RgbaImage,
};
use std::io::Cursor;

pub const MAX_SIDE: u32 = 1568;
pub const MIN_SIDE: u32 = 280;
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub extension: &'static str,
}

fn refused(reference: &str, why: impl std::fmt::Display) -> LlmError {
    LlmError::Unsupported(format!(
        "image {reference} can't be sent to TypeSafe: {why}"
    ))
}

fn within(width: u32, height: u32) -> bool {
    (MIN_SIDE..=MAX_SIDE).contains(&width) && (MIN_SIDE..=MAX_SIDE).contains(&height)
}

fn animated_png(bytes: &[u8]) -> bool {
    let idat = bytes.windows(4).position(|window| window == b"IDAT");
    let actl = bytes.windows(4).position(|window| window == b"acTL");
    matches!((actl, idat), (Some(actl), Some(idat)) if actl < idat)
}

pub fn prepare(reference: &str, image: &ImageData) -> Result<Prepared, LlmError> {
    let format = image::guess_format(&image.bytes).map_err(|e| refused(reference, e))?;
    let mut decoder = ImageReader::with_format(Cursor::new(&image.bytes), format)
        .into_decoder()
        .map_err(|e| refused(reference, e))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(|e| refused(reference, e))?;
    let (width, height) = decoded.dimensions();
    let passthrough = match format {
        ImageFormat::Png => !animated_png(&image.bytes),
        ImageFormat::Jpeg => true,
        _ => false,
    } && orientation == Orientation::NoTransforms
        && within(width, height)
        && image.bytes.len() <= MAX_FILE_BYTES;
    if passthrough {
        return Ok(match format {
            ImageFormat::Jpeg => Prepared {
                bytes: image.bytes.clone(),
                mime: "image/jpeg",
                extension: "jpg",
            },
            _ => Prepared {
                bytes: image.bytes.clone(),
                mime: "image/png",
                extension: "png",
            },
        });
    }
    decoded.apply_orientation(orientation);
    if decoded.width() > MAX_SIDE || decoded.height() > MAX_SIDE {
        decoded = decoded.resize(MAX_SIDE, MAX_SIDE, FilterType::Lanczos3);
    }
    if decoded.width() < MIN_SIDE || decoded.height() < MIN_SIDE {
        let mut canvas = RgbaImage::from_pixel(
            decoded.width().max(MIN_SIDE),
            decoded.height().max(MIN_SIDE),
            Rgba([255, 255, 255, 255]),
        );
        let x = (canvas.width() - decoded.width()) / 2;
        let y = (canvas.height() - decoded.height()) / 2;
        image::imageops::overlay(&mut canvas, &decoded.to_rgba8(), i64::from(x), i64::from(y));
        decoded = DynamicImage::ImageRgba8(canvas);
    }
    let mut bytes = Vec::new();
    let prepared = if format == ImageFormat::Jpeg {
        JpegEncoder::new_with_quality(&mut bytes, 90)
            .encode_image(&decoded.to_rgb8())
            .map_err(|e| refused(reference, e))?;
        Prepared {
            bytes,
            mime: "image/jpeg",
            extension: "jpg",
        }
    } else {
        decoded
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|e| refused(reference, e))?;
        Prepared {
            bytes,
            mime: "image/png",
            extension: "png",
        }
    };
    if prepared.bytes.len() > MAX_FILE_BYTES {
        return Err(refused(
            reference,
            format!(
                "it is {} bytes after conversion, and TypeSafe takes at most {MAX_FILE_BYTES}",
                prepared.bytes.len()
            ),
        ));
    }
    Ok(prepared)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            width,
            height,
            Rgba([10, 20, 30, 255]),
        ));
        let mut bytes = Vec::new();
        match format {
            ImageFormat::Jpeg => JpegEncoder::new(&mut bytes)
                .encode_image(&image.to_rgb8())
                .unwrap(),
            other => image.write_to(&mut Cursor::new(&mut bytes), other).unwrap(),
        }
        bytes
    }

    fn data(bytes: Vec<u8>) -> ImageData {
        ImageData {
            mime: "image/png".into(),
            bytes,
        }
    }

    fn size(prepared: &Prepared) -> (u32, u32) {
        image::load_from_memory(&prepared.bytes)
            .unwrap()
            .dimensions()
    }

    #[test]
    fn a_png_within_the_limits_is_sent_as_it_is() {
        let bytes = encoded(800, 600, ImageFormat::Png);
        let prepared = prepare("blob:a.png", &data(bytes.clone())).unwrap();
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.mime, "image/png");
    }

    #[test]
    fn a_gif_becomes_a_png() {
        let prepared = prepare("blob:a.gif", &data(encoded(400, 400, ImageFormat::Gif))).unwrap();
        assert_eq!(prepared.mime, "image/png");
        assert_eq!(
            image::guess_format(&prepared.bytes).unwrap(),
            ImageFormat::Png
        );
        assert_eq!(size(&prepared), (400, 400));
    }

    #[test]
    fn a_webp_becomes_a_png() {
        let prepared = prepare("blob:a.webp", &data(encoded(400, 300, ImageFormat::WebP))).unwrap();
        assert_eq!(
            image::guess_format(&prepared.bytes).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn a_large_image_is_scaled_to_the_longest_side_typesafe_takes() {
        let prepared =
            prepare("blob:a.jpg", &data(encoded(4000, 3000, ImageFormat::Jpeg))).unwrap();
        assert_eq!(prepared.mime, "image/jpeg");
        assert_eq!(size(&prepared), (1568, 1176));
    }

    #[test]
    fn a_small_image_is_padded_to_the_shortest_side_typesafe_takes() {
        let prepared = prepare("blob:a.png", &data(encoded(100, 100, ImageFormat::Png))).unwrap();
        assert_eq!(size(&prepared), (280, 280));
    }

    #[test]
    fn a_wide_strip_is_scaled_then_padded() {
        let prepared = prepare("blob:a.png", &data(encoded(5000, 300, ImageFormat::Png))).unwrap();
        assert_eq!(size(&prepared), (1568, 280));
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused_naming_the_ref() {
        let error = prepare("blob:broken.png", &data(b"not an image".to_vec())).unwrap_err();
        assert!(error.to_string().contains("blob:broken.png"), "{error}");
    }
}
