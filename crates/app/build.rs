//! The tray and window icons are authored as two JPEGs next to the source.
//!
//! Windows still needs an `.ico` resource to give the executable a file icon, so the
//! multi-size icon is generated here during the build instead of being checked in.
//! Replace `resources/opencap-idle.jpg` or `resources/opencap-recording.jpg` and rebuild;
//! there is no `.ico` to keep in sync.

use std::path::{Path, PathBuf};

/// Size ladder handed to Windows. It covers the tray at 100-250% scaling plus the
/// window and taskbar icons, so the shell never has to scale a mismatched frame.
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 128];
/// Frames at or above this size are stored as PNG to keep the resource small.
const PNG_FROM: u32 = 128;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
        let sources = [
            (101u32, manifest.join("resources/opencap-idle.jpg")),
            (102u32, manifest.join("resources/opencap-recording.jpg")),
        ];
        let mut rc = String::new();
        for (id, source) in sources {
            println!("cargo:rerun-if-changed={}", source.display());
            let icon = out.join(format!(
                "{}.ico",
                source.file_stem().unwrap().to_string_lossy()
            ));
            let bytes = scaled_icon(&source)
                .unwrap_or_else(|e| panic!("generating an icon from {}: {e}", source.display()));
            std::fs::write(&icon, bytes).unwrap();
            // rc.exe treats backslashes inside a quoted path as escapes, so hand it
            // forward slashes.
            let icon = icon.display().to_string().replace('\\', "/");
            rc.push_str(&format!("{id} ICON \"{icon}\"\n"));
        }
        let rc_path = out.join("opencap.rc");
        std::fs::write(&rc_path, rc).unwrap();
        embed_resource::compile(&rc_path, embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}

#[cfg(windows)]
fn scaled_icon(source: &Path) -> Result<Vec<u8>, String> {
    let image = image::open(source).map_err(|e| e.to_string())?;
    let limit = image.width().min(image.height());
    let mut frames = Vec::new();
    for size in SIZES.into_iter().filter(|size| *size <= limit) {
        // resize_to_fill keeps the aspect ratio and centre-crops a non-square source
        // instead of stretching it.
        let scaled = image.resize_to_fill(size, size, image::imageops::FilterType::Lanczos3);
        let payload = if size >= PNG_FROM {
            use image::ImageEncoder;
            let mut png = Vec::new();
            image::codecs::png::PngEncoder::new_with_quality(
                &mut std::io::Cursor::new(&mut png),
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(
                scaled.to_rgba8().as_raw(),
                size,
                size,
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| e.to_string())?;
            png
        } else {
            encode_dib(&scaled.to_rgba8(), size)
        };
        frames.push((size, payload));
    }
    if frames.is_empty() {
        return Err(format!("source is smaller than {}px", SIZES[0]));
    }
    Ok(build_ico(frames))
}

/// Uncompressed 32bpp DIB with the AND mask that `rc.exe` expects for small frames.
#[cfg(windows)]
fn encode_dib(rgba: &image::RgbaImage, size: u32) -> Vec<u8> {
    let width = size as usize;
    let height = size as usize;
    let xor_stride = width * 4;
    let mask_stride = width.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + xor_stride * height + mask_stride * height);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&((height * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((xor_stride * height) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for y in (0..height).rev() {
        for x in 0..width {
            let pixel = rgba.get_pixel(x as u32, y as u32).0;
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    let mask_start = out.len();
    out.resize(mask_start + mask_stride * height, 0);
    for y in 0..height {
        for x in 0..width {
            if rgba.get_pixel(x as u32, (height - 1 - y) as u32).0[3] < 128 {
                out[mask_start + y * mask_stride + (x >> 3)] |= 0x80 >> (x & 7);
            }
        }
    }
    out
}

#[cfg(windows)]
fn build_ico(frames: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * frames.len() as u32;
    for (size, payload) in &frames {
        let dimension = if *size >= 256 { 0 } else { *size as u8 };
        out.push(dimension);
        out.push(dimension);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += payload.len() as u32;
    }
    for (_, payload) in &frames {
        out.extend_from_slice(payload);
    }
    out
}
