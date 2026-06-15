//! Renders a QR pairing string into a GDK texture for display.
//!
//! whatsapp-rust hands us the raw pairing payload as a `String`; it is our job to
//! turn it into a scannable QR image. We rasterize the module matrix by hand into
//! an RGBA buffer (black modules on a white background with a quiet zone) and wrap
//! it in a `gdk::MemoryTexture`, avoiding a dependency on the `image` crate.

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use qrcode::{Color, QrCode};

/// Builds a GDK texture for `data`.
///
/// * `module_px` — side length, in pixels, of a single QR module.
/// * `quiet_modules` — width of the white border (quiet zone), in modules. The
///   QR spec recommends at least 4 for reliable scanning.
pub fn qr_texture(data: &str, module_px: i32, quiet_modules: i32) -> anyhow::Result<gdk::Texture> {
    let code = QrCode::new(data.as_bytes())?;
    let colors = code.to_colors();
    let modules = code.width() as i32; // matrix is `modules` x `modules`

    let total = modules + quiet_modules * 2;
    let size = (total * module_px) as usize;
    let stride = size * 4;

    // Start from an opaque white canvas; we only paint the dark modules.
    let mut buf = vec![255u8; stride * size];

    for my in 0..modules {
        for mx in 0..modules {
            if colors[(my * modules + mx) as usize] != Color::Dark {
                continue;
            }
            let px0 = ((mx + quiet_modules) * module_px) as usize;
            let py0 = ((my + quiet_modules) * module_px) as usize;
            for dy in 0..module_px as usize {
                let row = (py0 + dy) * stride;
                for dx in 0..module_px as usize {
                    let idx = row + (px0 + dx) * 4;
                    buf[idx] = 0; // R
                    buf[idx + 1] = 0; // G
                    buf[idx + 2] = 0; // B
                    buf[idx + 3] = 255; // A
                }
            }
        }
    }

    let bytes = glib::Bytes::from(&buf[..]);
    let texture = gdk::MemoryTexture::new(
        size as i32,
        size as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        stride,
    );
    Ok(texture.upcast())
}
