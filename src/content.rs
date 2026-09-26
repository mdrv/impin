//! Pin content: per source file, either delegate to the gpui asset pipeline
//! (PNG/JPEG/WebP/GIF/BMP decode + animation, SVG via the fork's resvg) or
//! decode in-process (AVIF/JXL, first frame) into a BGRA `RenderImage`.

use std::path::Path;
use std::sync::Arc;

use gpui::RenderImage;
use zenpixels::PixelFormat;

/// What a pin renders.
#[derive(Clone)]
pub enum PinImage {
    /// `img(path)` — the gpui asset pipeline handles decode (and animation).
    Asset,
    /// Pre-decoded BGRA frame(s) (AVIF/JXL first frame; animation is v0.2).
    Decoded(Arc<RenderImage>),
}

/// A loadable pin source: what to render plus the image's natural size
/// (feeds zoom math).
pub struct Loaded {
    pub render: PinImage,
    pub natural: (u32, u32),
}

pub fn load(path: &Path) -> anyhow::Result<Loaded> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("avif") => {
            let (render, natural) = avif(path)?;
            Ok(Loaded { render, natural })
        }
        Some("jxl") => {
            let (render, natural) = jxl(path)?;
            Ok(Loaded { render, natural })
        }
        Some("svg") => Ok(Loaded {
            render: PinImage::Asset,
            natural: svg_size(path)?,
        }),
        _ => Ok(Loaded {
            render: PinImage::Asset,
            natural: dims(path)?,
        }),
    }
}

/// Natural size for asset-pipeline formats (header read only).
pub fn dims(path: &Path) -> anyhow::Result<(u32, u32)> {
    image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()
        .map_err(|e| anyhow::anyhow!("reading image dims: {e}"))
}

/// Build the single-frame `RenderImage` gpui wants: its frame buffers are
/// BGRA, decoders hand us RGBA, so swap the channels into place.
fn decoded(img: image::RgbaImage) -> PinImage {
    let mut img = img;
    for px in img.pixels_mut() {
        px.0.swap(0, 2);
    }
    PinImage::Decoded(Arc::new(RenderImage::new(vec![image::Frame::new(img)])))
}

fn rgba8(
    w: usize,
    h: usize,
    px: impl Iterator<Item = [u8; 4]>,
) -> anyhow::Result<image::RgbaImage> {
    let mut buf = Vec::with_capacity(w * h * 4);
    for p in px {
        buf.extend_from_slice(&p);
    }
    image::RgbaImage::from_raw(w as u32, h as u32, buf)
        .ok_or_else(|| anyhow::anyhow!("decoder size mismatch ({w}x{h})"))
}

/// AVIF: decoded in-process (kornelski's avif-decode over rav1d, pure Rust);
/// first frame only in v0.1.
fn avif(path: &Path) -> anyhow::Result<(PinImage, (u32, u32))> {
    let bytes = std::fs::read(path)?;
    let buffer = zenavif::decode(&bytes)?;
    let (w, h) = (buffer.width(), buffer.height());
    let raw = buffer.copy_to_contiguous_bytes();
    let format = buffer.descriptor().format;
    let img = match format {
        PixelFormat::Rgba8 => image::RgbaImage::from_raw(w, h, raw)
            .ok_or_else(|| anyhow::anyhow!("AVIF buffer size mismatch"))?,
        PixelFormat::Rgb8 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(3).map(|p| [p[0], p[1], p[2], 255]),
        )?,
        PixelFormat::Bgra8 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(4).map(|p| [p[2], p[1], p[0], p[3]]),
        )?,
        PixelFormat::Rgbx8 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(4).map(|p| [p[0], p[1], p[2], 255]),
        )?,
        PixelFormat::Bgrx8 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(4).map(|p| [p[2], p[1], p[0], 255]),
        )?,
        PixelFormat::Gray8 => rgba8(
            w as usize,
            h as usize,
            raw.iter().map(|l| [*l, *l, *l, 255]),
        )?,
        PixelFormat::GrayA8 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(2).map(|p| [p[0], p[0], p[0], p[1]]),
        )?,
        // 10/12-bit sources arrive as native-endian u16; keep the top 8 bits.
        PixelFormat::Rgba16 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(8).map(|p| {
                [
                    (u16::from_le_bytes([p[0], p[1]]) >> 8) as u8,
                    (u16::from_le_bytes([p[2], p[3]]) >> 8) as u8,
                    (u16::from_le_bytes([p[4], p[5]]) >> 8) as u8,
                    (u16::from_le_bytes([p[6], p[7]]) >> 8) as u8,
                ]
            }),
        )?,
        PixelFormat::Rgb16 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(6).map(|p| {
                [
                    (u16::from_le_bytes([p[0], p[1]]) >> 8) as u8,
                    (u16::from_le_bytes([p[2], p[3]]) >> 8) as u8,
                    (u16::from_le_bytes([p[4], p[5]]) >> 8) as u8,
                    255,
                ]
            }),
        )?,
        PixelFormat::Gray16 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(2).map(|p| {
                let l = (u16::from_le_bytes([p[0], p[1]]) >> 8) as u8;
                [l, l, l, 255]
            }),
        )?,
        PixelFormat::GrayA16 => rgba8(
            w as usize,
            h as usize,
            raw.chunks_exact(4).map(|p| {
                let l = (u16::from_le_bytes([p[0], p[1]]) >> 8) as u8;
                [l, l, l, (u16::from_le_bytes([p[2], p[3]]) >> 8) as u8]
            }),
        )?,
        _ => {
            anyhow::bail!("unsupported AVIF pixel format {format:?} (HDR/float not supported yet)")
        }
    };
    Ok((decoded(img), (w, h)))
}

/// JXL: decoded in-process (jxl-oxide, pure Rust); first frame only in v0.1.
fn jxl(path: &Path) -> anyhow::Result<(PinImage, (u32, u32))> {
    let image =
        jxl_oxide::JxlImage::open_with_defaults(path).map_err(|e| anyhow::anyhow!("jxl: {e}"))?;
    let (w, h) = (image.width(), image.height());
    let fb = image
        .render_frame(0)
        .map_err(|e| anyhow::anyhow!("jxl: {e}"))?
        .image_all_channels();
    if (fb.width(), fb.height()) != (w as usize, h as usize) {
        anyhow::bail!(
            "JXL size mismatch ({}x{} vs {w}x{h})",
            fb.width(),
            fb.height()
        );
    }
    // Rendered samples are linear-ish f32 in [0, 1] per channel.
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let buf = fb.buf();
    let img = match fb.channels() {
        4 => rgba8(
            w as usize,
            h as usize,
            buf.chunks_exact(4)
                .map(|p| [to_u8(p[0]), to_u8(p[1]), to_u8(p[2]), to_u8(p[3])]),
        )?,
        3 => rgba8(
            w as usize,
            h as usize,
            buf.chunks_exact(3)
                .map(|p| [to_u8(p[0]), to_u8(p[1]), to_u8(p[2]), 255]),
        )?,
        1 => rgba8(
            w as usize,
            h as usize,
            buf.iter().map(|l| {
                let l = to_u8(*l);
                [l, l, l, 255]
            }),
        )?,
        other => anyhow::bail!("unsupported JXL channel count: {other}"),
    };
    Ok((decoded(img), (w, h)))
}

/// SVG renders natively via the fork's resvg; only the natural size is
/// needed here (zoom 1 = the fit size, per spec).
fn svg_size(path: &Path) -> anyhow::Result<(u32, u32)> {
    let text = std::fs::read_to_string(path)?;
    let tree = usvg::Tree::from_str(&text, &usvg::Options::default())?;
    let size = tree.size();
    Ok((
        size.width().round().max(1.0) as u32,
        size.height().round().max(1.0) as u32,
    ))
}
