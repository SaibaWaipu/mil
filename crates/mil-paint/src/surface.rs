//! A terminal-sized grid of styled cells plus out-of-band inline images.
//!
//! [`Surface`] is the intermediate representation between the HTML/CSS
//! renderer ([`crate`]) and the TUI compositor (`mil-tui`). It deliberately
//! contains no ratatui or crossterm types so that painting can be tested
//! headlessly and independently of the terminal backend.

use std::sync::Arc;

/// A CSS colour reduced to the 8-bit values a terminal can display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const BLACK: Self = Self::new(0, 0, 0);
    pub const WHITE: Self = Self::new(255, 255, 255);
}

/// Text attributes representable by virtually every terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub reversed: bool,
}

impl CellStyle {
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            reversed: false,
        }
    }
}

/// One terminal cell. `symbol` is a single grapheme cluster; wide glyphs
/// occupy two cells by duplicating the grapheme in the second cell with
/// `continuation` set so the compositor can skip it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub symbol: String,
    pub style: CellStyle,
    pub continuation: bool,
}

impl Cell {
    pub fn blank() -> Self {
        Self {
            symbol: " ".to_owned(),
            style: CellStyle::new(),
            continuation: false,
        }
    }

    pub fn glyph(symbol: impl Into<String>, style: CellStyle) -> Self {
        Self {
            symbol: symbol.into(),
            style,
            continuation: false,
        }
    }
}

/// An image placed at a cell-aligned rectangle. Pixels are PNG-encoded so
/// the TUI layer can hand them to ratatui-image without further conversion.
#[derive(Debug, Clone)]
pub struct PlacedImage {
    pub id: u64,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub png: Arc<[u8]>,
    /// Text shown when the terminal has no inline-image support.
    pub alt: String,
}

impl PlacedImage {
    pub fn new(
        id: u64,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        png: impl Into<Arc<[u8]>>,
        alt: impl Into<String>,
    ) -> Self {
        Self {
            id,
            x,
            y,
            width,
            height,
            png: png.into(),
            alt: alt.into(),
        }
    }

    /// Whether any part of the image intersects the given viewport rectangle.
    pub fn intersects(&self, vx: u16, vy: u16, vw: u16, vh: u16) -> bool {
        self.x < vx.saturating_add(vw)
            && vx < self.x.saturating_add(self.width)
            && self.y < vy.saturating_add(vh)
            && vy < self.y.saturating_add(self.height)
    }
}

/// Full document snapshot: a cell grid plus images layered on top.
#[derive(Debug, Clone)]
pub struct Surface {
    pub width: u16,
    pub height: u16,
    cells: Vec<Cell>,
    images: Vec<PlacedImage>,
}

impl Surface {
    pub fn new(width: u16, height: u16) -> Self {
        let len = usize::from(width) * usize::from(height);
        Self {
            width,
            height,
            cells: vec![Cell::blank(); len],
            images: Vec::new(),
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(usize::from(y) * usize::from(self.width) + usize::from(x))
    }

    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if let Some(i) = self.index(x, y) {
            self.cells[i] = cell;
        }
    }

    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        self.index(x, y).map(|i| &self.cells[i])
    }

    /// Mutable access to a single cell, if it is inside the surface.
    pub fn get_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        self.index(x, y).map(|i| &mut self.cells[i])
    }

    /// Set the background colour of every cell in a rectangle, leaving symbols
    /// and text attributes intact. Out-of-bounds coordinates are clipped.
    pub fn fill_bg_rect(&mut self, x: i32, y: i32, width: i32, height: i32, color: Rgb) {
        let x_end = x.saturating_add(width).min(i32::from(self.width));
        let y_end = y.saturating_add(height).min(i32::from(self.height));
        for row in y.max(0)..y_end {
            for col in x.max(0)..x_end {
                if let Some(cell) = self.get_mut(col as u16, row as u16) {
                    cell.style.bg = Some(color);
                }
            }
        }
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn images(&self) -> &[PlacedImage] {
        &self.images
    }

    pub fn push_image(&mut self, image: PlacedImage) {
        self.images.push(image);
    }

    pub fn clear_images(&mut self) {
        self.images.clear();
    }

    /// Fill a rectangle with blank cells. Used to reserve space for images so
    /// the compositor never overwrites them with stale text.
    pub fn clear_rect(&mut self, x: u16, y: u16, width: u16, height: u16) {
        for row in y..y.saturating_add(height).min(self.height) {
            for col in x..x.saturating_add(width).min(self.width) {
                if let Some(i) = self.index(col, row) {
                    self.cells[i] = Cell::blank();
                }
            }
        }
    }

    /// Like [`Surface::draw_grapheme`], but keeps the background colour already
    /// present in each destination cell so text composes over painted
    /// backgrounds instead of erasing them.
    pub fn draw_grapheme_merged(
        &mut self,
        x: u16,
        y: u16,
        grapheme: &str,
        style: CellStyle,
    ) -> u16 {
        let width = unicode_width::UnicodeWidthStr::width(grapheme).clamp(1, 2) as u16;
        if x >= self.width || y >= self.height {
            return 0;
        }
        let mut merged = self.get(x, y).map(|cell| cell.style).unwrap_or_default();
        merged.fg = style.fg;
        merged.bold = style.bold;
        merged.italic = style.italic;
        merged.underline = style.underline;
        merged.strikethrough = style.strikethrough;
        merged.reversed = style.reversed;
        self.set(x, y, Cell::glyph(grapheme.to_owned(), merged));
        if width == 2 && x + 1 < self.width {
            let mut continuation = self
                .get(x + 1, y)
                .map(|cell| cell.style)
                .unwrap_or_default();
            continuation.fg = style.fg;
            continuation.bold = style.bold;
            continuation.italic = style.italic;
            continuation.underline = style.underline;
            self.set(
                x + 1,
                y,
                Cell {
                    symbol: String::new(),
                    style: continuation,
                    continuation: true,
                },
            );
        }
        width
    }

    /// Draw a grapheme cluster, consuming one or two cells depending on its
    /// display width. Returns the number of cells consumed.
    pub fn draw_grapheme(&mut self, x: u16, y: u16, grapheme: &str, style: CellStyle) -> u16 {
        let width = unicode_width::UnicodeWidthStr::width(grapheme).clamp(1, 2) as u16;
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.set(x, y, Cell::glyph(grapheme.to_owned(), style));
        if width == 2 && x + 1 < self.width {
            self.set(
                x + 1,
                y,
                Cell {
                    symbol: String::new(),
                    style,
                    continuation: true,
                },
            );
        }
        width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_surface_is_fully_blank() {
        let s = Surface::new(4, 3);
        assert_eq!(s.cells().len(), 12);
        assert!(s.cells().iter().all(|c| c.symbol == " "));
    }

    #[test]
    fn wide_grapheme_consumes_two_cells() {
        let mut s = Surface::new(4, 1);
        let used = s.draw_grapheme(1, 0, "あ", CellStyle::new());
        assert_eq!(used, 2);
        assert_eq!(s.get(1, 0).unwrap().symbol, "あ");
        assert!(s.get(2, 0).unwrap().continuation);
        assert_eq!(s.get(3, 0).unwrap().symbol, " ");
    }

    #[test]
    fn out_of_bounds_writes_are_ignored() {
        let mut s = Surface::new(2, 2);
        s.set(9, 9, Cell::glyph("x", CellStyle::new()));
        assert!(s.get(0, 0).unwrap().symbol == " ");
    }

    #[test]
    fn clear_rect_does_not_wrap_rows() {
        let mut s = Surface::new(4, 2);
        for y in 0..2 {
            for x in 0..4 {
                s.set(x, y, Cell::glyph("x", CellStyle::new()));
            }
        }
        s.clear_rect(3, 0, 3, 1);
        assert_eq!(s.get(3, 0).unwrap().symbol, " ");
        assert_eq!(s.get(0, 1).unwrap().symbol, "x");
    }

    #[test]
    fn image_intersection_respects_viewport() {
        let img = PlacedImage::new(1, 5, 5, 4, 4, vec![], "alt");
        assert!(img.intersects(0, 0, 10, 10));
        assert!(!img.intersects(0, 0, 5, 5));
        assert!(!img.intersects(9, 9, 2, 2));
    }
}
