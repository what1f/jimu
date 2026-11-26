use std::{borrow::Cow, io::Cursor};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{AnimationDecoder, GenericImageView, ImageFormat, imageops::FilterType};
use jimu_ai::ImageContent;

/// Limits apply to the image file bytes before base64 encoding.
#[derive(Clone, Debug)]
pub struct ImageProcessConfig {
    /// Resize or re-encode oversized images. Validation always runs.
    pub auto_resize: bool,
    /// Longest side of a resized image, in pixels (default 2,000).
    pub max_dimension: u32,
    /// Maximum file size after resizing (default 3,500,000 bytes).
    pub max_encoded_bytes: usize,
}

impl Default for ImageProcessConfig {
    fn default() -> Self {
        Self {
            auto_resize: true,
            max_dimension: 2000,
            max_encoded_bytes: 3_500_000,
        }
    }
}

pub(crate) fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    image::guess_format(bytes).ok().filter(|format| {
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
        )
    })
}

pub(crate) struct ProcessedImage {
    pub content: ImageContent,
    pub original: (u32, u32),
    pub size: (u32, u32),
}

impl ProcessedImage {
    pub fn note(&self) -> String {
        let (w, h) = self.size;
        let (ow, oh) = self.original;
        let mime = &self.content.mime_type;
        if self.size == self.original {
            format!("{mime}, {w}×{h} pixels.")
        } else {
            format!(
                "{mime}, {w}×{h} pixels (original {ow}×{oh}). For original coordinates, multiply x by {ow}/{w} and y by {oh}/{h}."
            )
        }
    }
}

// Skip length-prefixed segments so EOI inside an embedded preview cannot end the main image.
fn jpeg_has_eoi(mut bytes: &[u8]) -> bool {
    let mut saw_scan = false;
    bytes = &bytes[2..];
    while let Some(marker) = bytes.iter().position(|&b| b == 0xff) {
        bytes = &bytes[marker..];
        let Some(code) = bytes.iter().position(|&b| b != 0xff) else {
            break;
        };
        let marker = bytes[code];
        bytes = &bytes[code + 1..];
        match marker {
            0xd9 => return saw_scan,
            0 | 1 | 0xd0..=0xd7 => continue, // stuffed byte, TEM, or restart
            _ => {
                let [hi, lo, ..] = *bytes else { break };
                let len = u16::from_be_bytes([hi, lo]) as usize;
                let Some(rest) = bytes.get(len..).filter(|_| len >= 2) else {
                    break;
                };
                saw_scan |= marker == 0xda;
                bytes = rest;
            }
        }
    }
    false
}

fn complete_png(mut chunks: &[u8]) -> bool {
    while let [a, b, c, d, ..] = *chunks {
        let len = u32::from_be_bytes([a, b, c, d]) as usize;
        let Some(end) = len.checked_add(8) else {
            return false;
        };
        let Some(chunk) = chunks.get(4..end) else {
            return false;
        };
        let Some((crc, rest)) = chunks[end..].split_first_chunk::<4>() else {
            return false;
        };
        if chunk.starts_with(b"IEND") {
            return len == 0 && crc == b"\xaeB\x60\x82";
        }
        chunks = rest;
    }
    false
}

pub(crate) fn process_image_bytes(
    bytes: &[u8],
    cfg: &ImageProcessConfig,
) -> Result<ProcessedImage, String> {
    let format = image_format(bytes).ok_or("Unsupported image. Use PNG, JPEG, GIF, or WebP.")?;
    let complete = match format {
        ImageFormat::Jpeg => jpeg_has_eoi(bytes),
        ImageFormat::WebP => {
            let size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            u64::from(size) + 8 <= bytes.len() as u64
        }
        ImageFormat::Png => complete_png(&bytes[8..]),
        ImageFormat::Gif => bytes.last() == Some(&0x3b),
        _ => unreachable!(),
    };
    if !complete {
        return Err("The image is incomplete or damaged.".into());
    }

    let decoded =
        image::load_from_memory(bytes).map_err(|e| format!("Could not decode image: {e}"))?;
    validate_frames(bytes, format).map_err(|e| format!("Could not decode image frame: {e}"))?;
    let original = decoded.dimensions();
    let long_side = original.0.max(original.1);
    let mut side = long_side.min(cfg.max_dimension);
    let (data, mime_type, size) =
        if !cfg.auto_resize || (side == long_side && bytes.len() <= cfg.max_encoded_bytes) {
            (STANDARD.encode(bytes), format.to_mime_type(), original)
        } else {
            loop {
                if side == 0 {
                    return Err("The image cannot fit within the configured size limit.".into());
                }
                let img = if side < long_side {
                    Cow::Owned(decoded.resize(side, side, FilterType::Triangle))
                } else {
                    Cow::Borrowed(&decoded)
                };
                let mut encoded = Cursor::new(Vec::new());
                img.write_to(&mut encoded, ImageFormat::Png)
                    .map_err(|e| format!("Could not encode PNG: {e}"))?;
                if encoded.get_ref().len() <= cfg.max_encoded_bytes {
                    break (
                        STANDARD.encode(encoded.into_inner()),
                        "image/png",
                        img.dimensions(),
                    );
                }
                let mut jpeg = Vec::new();
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
                    .encode_image(&img.to_rgb8())
                    .map_err(|e| format!("Could not encode JPEG: {e}"))?;
                if jpeg.len() <= cfg.max_encoded_bytes {
                    break (STANDARD.encode(jpeg), "image/jpeg", img.dimensions());
                }
                side /= 2;
            }
        };
    Ok(ProcessedImage {
        content: ImageContent {
            data,
            mime_type: mime_type.into(),
        },
        original,
        size,
    })
}

// load_from_memory decodes the default image. Check subsequent animation frames before forwarding the original bytes.
fn validate_frames(bytes: &[u8], format: ImageFormat) -> image::ImageResult<()> {
    let input = Cursor::new(bytes);
    let mut frames = match format {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(input)?.into_frames(),
        ImageFormat::WebP => {
            let decoder = image::codecs::webp::WebPDecoder::new(input)?;
            if !decoder.has_animation() {
                return Ok(());
            }
            decoder.into_frames()
        }
        ImageFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::new(input)?;
            if !decoder.is_apng()? {
                return Ok(());
            }
            decoder.apng()?.into_frames()
        }
        _ => return Ok(()),
    };
    frames.try_for_each(|frame| frame.map(drop))
}
