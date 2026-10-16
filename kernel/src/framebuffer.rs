//! Minimal framebuffer drawing that respects the firmware's pixel format.

use bootloader_api::info::{FrameBuffer, PixelFormat};

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const BACKGROUND: Self = Self::new(0x1f, 0x6f, 0x8b);
}

/// Fills the whole screen with one color.
pub fn fill(framebuffer: &mut FrameBuffer, color: Color) {
    let info = framebuffer.info();
    let buffer = framebuffer.buffer_mut();

    for y in 0..info.height {
        let row_start = y * info.stride * info.bytes_per_pixel;
        for x in 0..info.width {
            let offset = row_start + x * info.bytes_per_pixel;
            let Some(pixel) = buffer.get_mut(offset..offset + info.bytes_per_pixel) else {
                return;
            };
            write_pixel(pixel, info.pixel_format, color);
        }
    }
}

fn write_pixel(pixel: &mut [u8], format: PixelFormat, color: Color) {
    match format {
        PixelFormat::Rgb => {
            if let [r, g, b, ..] = pixel {
                (*r, *g, *b) = (color.r, color.g, color.b);
            }
        }
        PixelFormat::Bgr => {
            if let [b, g, r, ..] = pixel {
                (*b, *g, *r) = (color.b, color.g, color.r);
            }
        }
        PixelFormat::U8 => {
            if let [v, ..] = pixel {
                let sum = u16::from(color.r) + u16::from(color.g) + u16::from(color.b);
                *v = (sum / 3) as u8;
            }
        }
        _ => {} // unknown layouts: draw nothing rather than corrupt the screen
    }
}
