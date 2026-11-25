//! Painting backend for `mil`.
//!
//! Blitz resolves HTML/CSS into absolute pixel rectangles and text runs; this
//! crate turns those drawing commands into a terminal-sized [`Surface`] of
//! styled cells and inline images. Nothing here depends on a terminal backend,
//! which makes the output snapshot-testable.

pub mod surface;
pub mod terminal;

pub use surface::{Cell, CellStyle, PlacedImage, Rgb, Surface};
pub use terminal::{RecordedFill, RecordedImage, TerminalScene};

/// Geometry of one terminal cell and the scale used when painting a document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaintConfig {
    /// Cell width in device pixels.
    pub cell_width: u16,
    /// Cell height in device pixels.
    pub cell_height: u16,
    /// HiDPI scale passed to `blitz_paint::paint_scene`.
    pub scale: f64,
}

impl Default for PaintConfig {
    fn default() -> Self {
        Self {
            cell_width: 8,
            cell_height: 16,
            scale: 1.0,
        }
    }
}

impl PaintConfig {
    /// Horizontal device pixels per terminal cell.
    pub fn pixel_width(&self) -> f64 {
        f64::from(self.cell_width) * self.scale
    }

    /// Vertical device pixels per terminal cell.
    pub fn pixel_height(&self) -> f64 {
        f64::from(self.cell_height) * self.scale
    }
}
