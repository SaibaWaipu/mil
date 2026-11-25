//! Composite a [`Page`] into a terminal [`Surface`].
//!
//! Blitz paints glyphs as font glyph ids, which cannot be mapped back to source
//! text. We therefore run `blitz-paint` only to collect backgrounds and images,
//! and lay out text ourselves from the DOM's resolved inline layouts.

use std::io::Cursor;

use anyhow::Result;
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use mil_paint::{CellStyle, PaintConfig, PlacedImage, Rgb, Surface, terminal::TerminalScene};
use peniko::{ImageAlphaType, ImageFormat};
use unicode_segmentation::UnicodeSegmentation;

use crate::page::Page;

/// Paint `page` into a `width_cells` by `height_cells` terminal surface.
pub fn render_page(
    page: &Page,
    config: &PaintConfig,
    width_cells: u16,
    height_cells: u16,
) -> Result<Surface> {
    let pixel_width = config.pixel_width().max(1.0);
    let pixel_height = config.pixel_height().max(1.0);
    let width_px = (f64::from(width_cells) * pixel_width).round().max(1.0) as u32;
    let height_px = (f64::from(height_cells) * pixel_height).round().max(1.0) as u32;

    let mut scene = TerminalScene::new();
    blitz_paint::paint_scene(&mut scene, &page.doc, config.scale, width_px, height_px);

    let mut surface = Surface::new(width_cells, height_cells);

    // Backgrounds first, so text can be merged over them.
    for fill in scene.fills() {
        let x = (fill.x / pixel_width).floor() as i32;
        let y = (fill.y / pixel_height).floor() as i32;
        let width = (fill.width / pixel_width).ceil() as i32;
        let height = (fill.height / pixel_height).ceil() as i32;
        surface.fill_bg_rect(x, y, width, height, to_rgb(fill.color.components));
    }

    // Source text from the DOM, placed into cells in reading order.
    //
    // A CSS pixel advance is not the same width as a terminal cell, so the
    // absolute x coordinate of each inline span cannot be used verbatim:
    // adjacent spans would overlap. Instead, start each visual line at the
    // first span's indent and place subsequent spans contiguously.
    let mut last_row: Option<u16> = None;
    let mut next_col: u16 = 0;
    for run in page.text_runs() {
        let mut row = (f64::from(run.y) / pixel_height).floor().max(0.0) as u16;
        let indent = (f64::from(run.x) / pixel_width).floor().max(0.0) as u16;
        let mut col = match last_row {
            Some(previous) if previous == row => next_col,
            _ => indent,
        };
        let style = CellStyle {
            fg: run.style.color,
            bold: run.style.bold,
            italic: run.style.italic,
            underline: run.style.underline,
            ..CellStyle::new()
        };
        for grapheme in run.text.graphemes(true) {
            if grapheme == "\n" || grapheme == "\r" {
                row = row.saturating_add(1);
                col = indent;
                continue;
            }
            if col >= width_cells || row >= height_cells {
                break;
            }
            if grapheme.chars().all(char::is_whitespace) {
                col = col.saturating_add(1);
                continue;
            }
            let used = surface.draw_grapheme_merged(col, row, grapheme, style);
            col = col.saturating_add(used.max(1));
            if col >= width_cells {
                col = 0;
                row = row.saturating_add(1);
            }
        }
        next_col = col;
        last_row = Some(row);
    }

    // Images last: they occupy cells exclusively and carry PNG pixels.
    for (index, image) in scene.images().iter().enumerate() {
        let rgba = to_rgba(image);
        if rgba.len() != image.image_width as usize * image.image_height as usize * 4 {
            continue;
        }
        let mut png = Cursor::new(Vec::new());
        PngEncoder::new(&mut png).write_image(
            &rgba,
            image.image_width,
            image.image_height,
            ExtendedColorType::Rgba8,
        )?;

        let x0 = (image.x / pixel_width).floor().max(0.0) as u16;
        let y0 = (image.y / pixel_height).floor().max(0.0) as u16;
        if x0 >= width_cells || y0 >= height_cells {
            continue;
        }
        let x1 = ((image.x + image.width) / pixel_width).ceil().max(0.0) as u16;
        let y1 = ((image.y + image.height) / pixel_height).ceil().max(0.0) as u16;
        let width = (x1.saturating_sub(x0)).clamp(1, width_cells - x0);
        let height = (y1.saturating_sub(y0)).clamp(1, height_cells - y0);
        surface.clear_rect(x0, y0, width, height);
        surface.push_image(PlacedImage::new(
            index as u64,
            x0,
            y0,
            width,
            height,
            png.into_inner(),
            String::new(),
        ));
    }

    Ok(surface)
}

fn to_rgb(components: [f32; 4]) -> Rgb {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb::new(
        byte(components[0]),
        byte(components[1]),
        byte(components[2]),
    )
}

/// Normalise a recorded paint image into straight-alpha RGBA8 bytes.
fn to_rgba(image: &mil_paint::RecordedImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.image_width as usize * image.image_height as usize * 4);
    let swap_channels = matches!(image.format, ImageFormat::Bgra8);
    let premultiplied = matches!(image.alpha_type, ImageAlphaType::AlphaPremultiplied);
    for pixel in image.pixels.chunks_exact(4) {
        let (r, g, b, a) = if swap_channels {
            (pixel[2], pixel[1], pixel[0], pixel[3])
        } else {
            (pixel[0], pixel[1], pixel[2], pixel[3])
        };
        let (r, g, b) = if premultiplied && a > 0 {
            let unpremultiply =
                |channel: u8| ((u16::from(channel) * 255 / u16::from(a)).min(255)) as u8;
            (unpremultiply(r), unpremultiply(g), unpremultiply(b))
        } else {
            (r, g, b)
        };
        let a = (f32::from(a) * image.sampler_alpha.clamp(0.0, 1.0)).round() as u8;
        out.extend_from_slice(&[r, g, b, a]);
    }
    out
}
