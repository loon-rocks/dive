use crate::image::ImageData;
use crate::LlmError;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgba, RgbaImage};
use std::collections::BTreeMap;
use std::io::Cursor;

pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_side: u32,
    pub crowded: Option<(usize, u32)>,
    pub min_side: u32,
    pub max_bytes: usize,
    pub keeps: &'static [ImageFormat],
    pub still_and_upright: bool,
}

pub const TYPESAFE: Limits = Limits {
    max_side: 1568,
    crowded: None,
    min_side: 280,
    max_bytes: 16 * 1024 * 1024,
    keeps: &[ImageFormat::Png, ImageFormat::Jpeg],
    still_and_upright: true,
};

pub const CHAT: Limits = Limits {
    max_side: 8000,
    crowded: Some((20, 2000)),
    min_side: 1,
    max_bytes: 5 * 1024 * 1024 / 4 * 3,
    keeps: &[
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::WebP,
    ],
    still_and_upright: false,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub bytes: bytes::Bytes,
    pub mime: &'static str,
    pub extension: &'static str,
}

impl Limits {
    fn side(&self, count: usize) -> u32 {
        match self.crowded {
            Some((over, side)) if count > over => side.min(self.max_side),
            _ => self.max_side,
        }
    }
}

fn refused(reference: &str, why: impl std::fmt::Display) -> LlmError {
    LlmError::Unsupported(format!("image {reference} can't be sent: {why}"))
}

fn animated_png(bytes: &[u8]) -> bool {
    let idat = bytes.windows(4).position(|window| window == b"IDAT");
    let actl = bytes.windows(4).position(|window| window == b"acTL");
    matches!((actl, idat), (Some(actl), Some(idat)) if actl < idat)
}

fn extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "jpg",
        other => other.extensions_str().first().copied().unwrap_or("png"),
    }
}

fn encode(reference: &str, image: &DynamicImage, jpeg: bool) -> Result<Prepared, LlmError> {
    let mut bytes = Vec::new();
    if jpeg {
        JpegEncoder::new_with_quality(&mut bytes, 90)
            .encode_image(&image.to_rgb8())
            .map_err(|e| refused(reference, e))?;
        return Ok(Prepared {
            bytes: bytes.into(),
            mime: "image/jpeg",
            extension: "jpg",
        });
    }
    image
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|e| refused(reference, e))?;
    Ok(Prepared {
        bytes: bytes.into(),
        mime: "image/png",
        extension: "png",
    })
}

fn pad(image: DynamicImage, min_side: u32) -> DynamicImage {
    if image.width() >= min_side && image.height() >= min_side {
        return image;
    }
    let mut canvas = RgbaImage::from_pixel(
        image.width().max(min_side),
        image.height().max(min_side),
        Rgba([255, 255, 255, 255]),
    );
    let x = (canvas.width() - image.width()) / 2;
    let y = (canvas.height() - image.height()) / 2;
    image::imageops::overlay(&mut canvas, &image.to_rgba8(), i64::from(x), i64::from(y));
    DynamicImage::ImageRgba8(canvas)
}

pub fn fit(
    reference: &str,
    image: &ImageData,
    limits: &Limits,
    count: usize,
) -> Result<Prepared, LlmError> {
    let format = image::guess_format(&image.bytes).map_err(|e| refused(reference, e))?;
    let mut decoder = ImageReader::with_format(Cursor::new(&image.bytes), format)
        .into_decoder()
        .map_err(|e| refused(reference, e))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (width, height) = decoder.dimensions();
    let side = limits.side(count);
    let fits = |width: u32, height: u32| {
        (limits.min_side..=side).contains(&width) && (limits.min_side..=side).contains(&height)
    };
    let passthrough = limits.keeps.contains(&format)
        && !(limits.still_and_upright
            && (orientation != Orientation::NoTransforms
                || (format == ImageFormat::Png && animated_png(&image.bytes))))
        && fits(width, height)
        && image.bytes.len() <= limits.max_bytes;
    if passthrough {
        return Ok(Prepared {
            bytes: image.bytes.clone(),
            mime: format.to_mime_type(),
            extension: extension(format),
        });
    }
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(|e| refused(reference, e))?;
    decoded.apply_orientation(orientation);
    let jpeg = format == ImageFormat::Jpeg;
    let mut longest = decoded.width().max(decoded.height()).min(side);
    loop {
        let scaled = if decoded.width() > longest || decoded.height() > longest {
            decoded.resize(longest, longest, FilterType::Lanczos3)
        } else {
            decoded.clone()
        };
        let prepared = encode(reference, &pad(scaled, limits.min_side), jpeg)?;
        if prepared.bytes.len() <= limits.max_bytes {
            return Ok(prepared);
        }
        let shrink = (limits.max_bytes as f64 / prepared.bytes.len() as f64).sqrt() * 0.9;
        let next = (f64::from(longest) * shrink) as u32;
        if next < limits.min_side.max(64) {
            return Err(refused(
                reference,
                format!(
                    "it is {} bytes after shrinking, and the provider takes at most {}",
                    prepared.bytes.len(),
                    limits.max_bytes
                ),
            ));
        }
        longest = next;
    }
}

pub fn fit_all(
    images: &BTreeMap<String, ImageData>,
    limits: &Limits,
) -> BTreeMap<String, ImageData> {
    images
        .iter()
        .map(|(reference, image)| {
            let fitted = match fit(reference, image, limits, images.len()) {
                Ok(prepared) => ImageData {
                    mime: prepared.mime.to_string(),
                    bytes: prepared.bytes,
                },
                Err(error) => {
                    tracing::warn!(file = %reference, %error, "image sent as it is");
                    image.clone()
                }
            };
            (reference.clone(), fitted)
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::GenericImageView;

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
            bytes: bytes.into(),
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
        let prepared = fit("blob:a.png", &data(bytes.clone()), &TYPESAFE, 1).unwrap();
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.mime, "image/png");
    }

    #[test]
    fn a_gif_becomes_a_png() {
        let prepared = fit(
            "blob:a.gif",
            &data(encoded(400, 400, ImageFormat::Gif)),
            &TYPESAFE,
            1,
        )
        .unwrap();
        assert_eq!(prepared.mime, "image/png");
        assert_eq!(
            image::guess_format(&prepared.bytes).unwrap(),
            ImageFormat::Png
        );
        assert_eq!(size(&prepared), (400, 400));
    }

    #[test]
    fn a_webp_becomes_a_png() {
        let prepared = fit(
            "blob:a.webp",
            &data(encoded(400, 300, ImageFormat::WebP)),
            &TYPESAFE,
            1,
        )
        .unwrap();
        assert_eq!(
            image::guess_format(&prepared.bytes).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn a_large_image_is_scaled_to_the_longest_side_typesafe_takes() {
        let prepared = fit(
            "blob:a.jpg",
            &data(encoded(4000, 3000, ImageFormat::Jpeg)),
            &TYPESAFE,
            1,
        )
        .unwrap();
        assert_eq!(prepared.mime, "image/jpeg");
        assert_eq!(size(&prepared), (1568, 1176));
    }

    #[test]
    fn a_small_image_is_padded_to_the_shortest_side_typesafe_takes() {
        let prepared = fit(
            "blob:a.png",
            &data(encoded(100, 100, ImageFormat::Png)),
            &TYPESAFE,
            1,
        )
        .unwrap();
        assert_eq!(size(&prepared), (280, 280));
    }

    #[test]
    fn a_wide_strip_is_scaled_then_padded() {
        let prepared = fit(
            "blob:a.png",
            &data(encoded(5000, 300, ImageFormat::Png)),
            &TYPESAFE,
            1,
        )
        .unwrap();
        assert_eq!(size(&prepared), (1568, 280));
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused_naming_the_ref() {
        let error = fit(
            "blob:broken.png",
            &data(b"not an image".to_vec()),
            &TYPESAFE,
            1,
        )
        .unwrap_err();
        assert!(error.to_string().contains("blob:broken.png"), "{error}");
    }

    fn noise(width: u32, height: u32) -> Vec<u8> {
        let mut seed = 7u32;
        let image = RgbaImage::from_fn(width, height, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [r, g, b, _] = seed.to_le_bytes();
            Rgba([r, g, b, 255])
        });
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn a_tall_screenshot_is_shrunk_to_the_chat_limit_keeping_its_shape() {
        let prepared = fit(
            "blob:page.png",
            &data(encoded(1185, 10696, ImageFormat::Png)),
            &CHAT,
            1,
        )
        .unwrap();
        assert_eq!(prepared.mime, "image/png");
        assert_eq!(size(&prepared), (886, 8000));
    }

    #[test]
    fn a_chat_image_within_the_limits_is_sent_as_it_is() {
        let bytes = encoded(1200, 900, ImageFormat::WebP);
        let prepared = fit("blob:a.webp", &data(bytes.clone()), &CHAT, 1).unwrap();
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.mime, "image/webp");
    }

    #[test]
    fn many_chat_images_are_held_to_the_smaller_side() {
        let prepared = fit(
            "blob:a.jpg",
            &data(encoded(3000, 1500, ImageFormat::Jpeg)),
            &CHAT,
            21,
        )
        .unwrap();
        assert_eq!(prepared.mime, "image/jpeg");
        assert_eq!(size(&prepared), (2000, 1000));
    }

    #[test]
    fn a_chat_image_over_the_byte_limit_is_shrunk_until_it_fits() {
        let prepared = fit("blob:noise.png", &data(noise(1600, 1200)), &CHAT, 1).unwrap();
        let (width, height) = size(&prepared);
        assert!(prepared.bytes.len() <= CHAT.max_bytes);
        assert!(
            width < 1600 && (width * 3).abs_diff(height * 4) <= 4,
            "{width}x{height}"
        );
    }

    #[test]
    fn an_image_that_cant_be_read_is_sent_to_a_chat_model_as_it_is() {
        let images = BTreeMap::from([("blob:broken.png".to_string(), data(b"nope".to_vec()))]);
        assert_eq!(fit_all(&images, &CHAT), images);
    }
}
