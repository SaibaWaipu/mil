//! Conversion from [`mil_paint::Surface`] cells into a ratatui [`Buffer`].
//!
//! The conversion is a straight row/column copy with a vertical offset. It
//! skips continuation cells (the second half of a wide glyph), maps CSS RGB
//! colours to truecolour, and never indexes outside the destination buffer.

use mil_paint::surface::{CellStyle, Surface};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Convert a fractional scroll offset into whole cell rows.
///
/// `NaN` and non-positive values become `0`; very large (including infinite)
/// values saturate at `u16::MAX` through the float-to-integer cast.
pub fn scroll_rows(scroll_y: f64) -> u16 {
    if scroll_y.is_nan() || scroll_y <= 0.0 {
        0
    } else {
        scroll_y.round() as u16
    }
}

/// Convert `scroll_y` to rows, first clamping it to the document's scrollable
/// range so scrolling past the end cannot blank the viewport.
///
/// A nonsensical `scroll_max_y` (`NaN` or negative) means "no known bound":
/// only the lower bound is applied. `f64::clamp` itself panics on `NaN`
/// bounds, hence the explicit guard.
pub(crate) fn clamped_scroll_rows(scroll_y: f64, scroll_max_y: f64) -> u16 {
    let bounded = if scroll_max_y.is_nan() || scroll_max_y < 0.0 {
        scroll_y.max(0.0)
    } else {
        scroll_y.clamp(0.0, scroll_max_y)
    };
    scroll_rows(bounded)
}

/// Copy the visible portion of `surface` into `buf` at `area`.
///
/// `scroll_rows` selects the first document row to draw; rows outside the
/// document or outside `area` are ignored. The caller is responsible for
/// clearing `area` first if stale cells from a previous frame must be removed.
pub fn render_surface(surface: &Surface, buf: &mut Buffer, area: Rect, scroll_rows: u16) {
    if area.is_empty() || surface.width == 0 {
        return;
    }
    let first_row = scroll_rows.min(surface.height);
    let columns = area.width.min(surface.width);
    let rows = area.height.min(surface.height.saturating_sub(first_row));
    let cells = surface.cells();

    for row in 0..rows {
        let source_row = first_row + row;
        let source_offset = usize::from(source_row) * usize::from(surface.width);
        let Some(target_y) = area.y.checked_add(row) else {
            break;
        };
        for column in 0..columns {
            let cell = &cells[source_offset + usize::from(column)];
            if cell.continuation || cell.symbol.is_empty() {
                continue;
            }
            let Some(target_x) = area.x.checked_add(column) else {
                break;
            };
            let Some(target) = buf.cell_mut((target_x, target_y)) else {
                continue;
            };
            target.set_symbol(&cell.symbol);
            target.set_style(style_for(cell.style));
        }
    }
}

/// Fill a rectangle with plain spaces so widgets cannot leave stale glyphs.
pub fn clear_area(buf: &mut Buffer, area: Rect) {
    if area.is_empty() {
        return;
    }
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(" ");
                cell.set_style(Style::default());
            }
        }
    }
}

fn style_for(style: CellStyle) -> Style {
    let mut result = Style::new();
    if let Some(fg) = style.fg {
        result = result.fg(Color::Rgb(fg.r, fg.g, fg.b));
    }
    if let Some(bg) = style.bg {
        result = result.bg(Color::Rgb(bg.r, bg.g, bg.b));
    }
    let mut modifier = Modifier::empty();
    if style.bold {
        modifier |= Modifier::BOLD;
    }
    if style.italic {
        modifier |= Modifier::ITALIC;
    }
    if style.underline {
        modifier |= Modifier::UNDERLINED;
    }
    if style.strikethrough {
        modifier |= Modifier::CROSSED_OUT;
    }
    if style.reversed {
        modifier |= Modifier::REVERSED;
    }
    result.add_modifier(modifier)
}

#[cfg(test)]
mod tests {
    use mil_paint::{Cell, CellStyle, Rgb, Surface};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use super::*;

    #[test]
    fn copies_cells_with_style_and_skips_continuations() {
        let mut surface = Surface::new(4, 1);
        let style = CellStyle {
            fg: Some(Rgb::new(1, 2, 3)),
            bg: Some(Rgb::new(4, 5, 6)),
            bold: true,
            underline: true,
            reversed: true,
            ..CellStyle::new()
        };
        surface.draw_grapheme(0, 0, "あ", style);
        surface.set(2, 0, Cell::glyph("x", style));

        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        render_surface(&surface, &mut buf, Rect::new(0, 0, 4, 1), 0);

        assert_eq!(buf[(0, 0)].symbol(), "あ");
        // The continuation cell must not overwrite the next grapheme slot.
        assert_eq!(buf[(2, 0)].symbol(), "x");
        let cell = &buf[(2, 0)];
        assert_eq!(cell.fg, Color::Rgb(1, 2, 3));
        assert_eq!(cell.bg, Color::Rgb(4, 5, 6));
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(cell.modifier.contains(Modifier::UNDERLINED));
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn scroll_and_crop_select_only_visible_rows() {
        let mut surface = Surface::new(3, 3);
        for row in 0..3 {
            surface.set(
                0,
                row,
                Cell::glyph(char::from(b'a' + row as u8), CellStyle::new()),
            );
        }
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 2));
        render_surface(&surface, &mut buf, Rect::new(0, 0, 2, 2), 1);
        assert_eq!(buf[(0, 0)].symbol(), "b");
        assert_eq!(buf[(0, 1)].symbol(), "c");
    }

    #[test]
    fn offset_area_and_oversized_scroll_are_safe() {
        let mut surface = Surface::new(2, 1);
        surface.set(0, 0, Cell::glyph("x", CellStyle::new()));
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 3));
        render_surface(&surface, &mut buf, Rect::new(2, 1, 4, 2), 99);
        assert_eq!(buf[(2, 1)].symbol(), " ");
        assert_eq!(buf[(5, 2)].symbol(), " ");
    }

    #[test]
    fn integration_with_test_backend() {
        let mut surface = Surface::new(4, 1);
        surface.set(0, 0, Cell::glyph("m", CellStyle::new()));
        surface.set(1, 0, Cell::glyph("i", CellStyle::new()));
        let mut terminal = Terminal::new(TestBackend::new(4, 1)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                clear_area(frame.buffer_mut(), area);
                render_surface(&surface, frame.buffer_mut(), area, 0);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "m");
        assert_eq!(terminal.backend().buffer()[(1, 0)].symbol(), "i");
    }

    #[test]
    fn scroll_rows_rounds_and_clamps() {
        assert_eq!(scroll_rows(-3.0), 0);
        assert_eq!(scroll_rows(f64::NAN), 0);
        assert_eq!(scroll_rows(1.4), 1);
        assert_eq!(scroll_rows(1.6), 2);
        assert_eq!(scroll_rows(f64::INFINITY), u16::MAX);
    }

    #[test]
    fn clamped_scroll_rows_respects_the_document_bound() {
        assert_eq!(clamped_scroll_rows(50.0, 10.0), 10);
        assert_eq!(clamped_scroll_rows(-5.0, 10.0), 0);
        assert_eq!(clamped_scroll_rows(5.0, 0.0), 0);
        assert_eq!(clamped_scroll_rows(f64::INFINITY, 10.0), 10);
        assert_eq!(clamped_scroll_rows(f64::NAN, 10.0), 0);
        assert_eq!(clamped_scroll_rows(5.0, f64::NAN), 5);
        assert_eq!(clamped_scroll_rows(5.0, -1.0), 5);
    }

    #[test]
    fn clipped_wide_glyph_does_not_touch_neighbouring_cells() {
        let mut surface = Surface::new(3, 2);
        surface.draw_grapheme(1, 0, "あ", CellStyle::new());
        surface.set(0, 1, Cell::glyph("z", CellStyle::new()));
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 2));
        render_surface(&surface, &mut buf, Rect::new(0, 0, 2, 1), 0);
        assert_eq!(buf[(1, 0)].symbol(), "あ");
        // The continuation cell is outside `area` and must not be written.
        assert_eq!(buf[(2, 0)].symbol(), " ");
        assert_eq!(buf[(0, 1)].symbol(), " ");
    }

    #[test]
    fn empty_surfaces_and_areas_are_noops() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 3));
        let area = buf.area;
        let zero_width = Surface::new(0, 3);
        let zero_height = Surface::new(4, 0);
        render_surface(&zero_width, &mut buf, area, 1);
        render_surface(&zero_height, &mut buf, area, 1);
        render_surface(&zero_height, &mut buf, Rect::ZERO, 0);
        assert!(buf.content.iter().all(|cell| cell.symbol() == " "));
    }
}
