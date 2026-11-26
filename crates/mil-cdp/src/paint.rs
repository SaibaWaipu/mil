//! Mapping document-pixel rectangles from the JS walker onto terminal cells.
//!
//! The layout rules here mirror `mil-dom`'s renderer: DOM order is not reading
//! order once CSS positions elements, a CSS-pixel advance is not a terminal
//! cell, and fractional baselines must not split one visual line in two.

use mil_paint::{CellStyle, Surface};
use unicode_segmentation::UnicodeSegmentation;

use crate::color::parse_color;
use crate::{CdpConfig, CdpLink, CdpSpan};

/// Maximum surface height. A tall page must not allocate an unbounded grid.
pub(crate) const MAX_ROWS: u16 = 1024;

/// Anything beyond this in a `getBoundingClientRect` result is junk (hidden
/// dropdowns, off-screen decorations, or a broken layout engine).
const MAX_COORD_PX: f64 = 1_000_000.0;

/// An image large enough to be captured as a PNG. Cell coordinates are kept
/// alongside document pixels because the screenshot clip uses the latter.
/// `alt` is populated only when it is safe to draw as a fallback (see
/// [`paint_spans`]); otherwise it stays empty.
#[derive(Debug, Clone)]
pub(crate) struct ImageRegion {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub cx: u16,
    pub cy: u16,
    pub cw: u16,
    pub ch: u16,
    pub alt: String,
}

/// Result of placing text and reserving image areas.
pub(crate) struct Painted {
    pub surface: Surface,
    pub links: Vec<CdpLink>,
    pub images: Vec<ImageRegion>,
}

/// Reject non-finite, negative, or absurdly large rectangles before they reach
/// cell conversion; otherwise `NaN`/`inf` silently become column 0 and paint
/// garbage at the top-left of the page.
fn valid_rect(span: &CdpSpan) -> bool {
    [span.x, span.y, span.w, span.h]
        .iter()
        .all(|value| value.is_finite() && *value >= 0.0 && *value <= MAX_COORD_PX)
}

/// Convert a document pixel position to a cell index.
pub(crate) fn cell_at(pixel: f64, cell_size: u32) -> u16 {
    if !pixel.is_finite() || pixel <= 0.0 {
        return 0;
    }
    let value = (pixel / f64::from(cell_size)).floor();
    if value > f64::from(u16::MAX) {
        u16::MAX
    } else {
        value as u16
    }
}

/// Number of cells needed to cover a pixel extent (at least one).
pub(crate) fn cells_for(pixels: f64, cell_size: u32) -> u16 {
    if !pixels.is_finite() || pixels <= 0.0 {
        return 1;
    }
    let value = (pixels / f64::from(cell_size)).ceil();
    if value < 1.0 {
        1
    } else if value > f64::from(u16::MAX) {
        u16::MAX
    } else {
        value as u16
    }
}

fn style_for(span: &CdpSpan) -> CellStyle {
    CellStyle {
        fg: parse_color(span.fg.as_deref()),
        bg: parse_color(span.bg.as_deref()),
        bold: span.bold.unwrap_or(false),
        italic: span.italic.unwrap_or(false),
        underline: span.underline.unwrap_or(false),
        ..CellStyle::new()
    }
}

/// Height of a full-document surface: at least the viewport, but never more
/// than [`MAX_ROWS`] rows. The TUI scrolls this surface like the Blitz one.
pub(crate) fn surface_height(document_px: f64, viewport_rows: u16, cell_height: u32) -> u16 {
    let needed = cells_for(document_px, cell_height).min(MAX_ROWS);
    needed.max(viewport_rows.min(MAX_ROWS))
}

/// Whether a rectangle already contains readable text. Used before reserving
/// an image area so a fallback alt label can never overwrite real content.
fn rect_has_text(surface: &Surface, x: u16, y: u16, width: u16, height: u16) -> bool {
    let last_row = y.saturating_add(height).min(surface.height);
    let last_col = x.saturating_add(width).min(surface.width);
    for row in y..last_row {
        for col in x..last_col {
            if surface
                .get(col, row)
                .is_some_and(|cell| !cell.continuation && !cell.symbol.trim().is_empty())
            {
                return true;
            }
        }
    }
    false
}

/// Paint all spans into a fresh surface, in visual reading order.
///
/// Text runs are sorted by (row, x); runs on the same visual line continue at
/// the previous run's end column only when their baselines are close *and*
/// their x positions are contiguous. A run starting past the right edge is
/// skipped in its entirety instead of leaking onto the next row. Images are
/// painted last and exclusively own their cells.
pub(crate) fn paint_spans(
    spans: &[CdpSpan],
    config: &CdpConfig,
    cols: u16,
    height: u16,
) -> Painted {
    let mut surface = Surface::new(cols, height);
    let mut links = Vec::new();
    let mut images = Vec::new();

    // DOM order is not reading order once CSS positions elements: visit runs
    // top-to-bottom, left-to-right so continuation state follows what a human
    // sees rather than the order the JS walker happened to emit spans in.
    let pixel_height = f64::from(config.cell_height);
    let mut runs: Vec<&CdpSpan> = spans
        .iter()
        .filter(|span| span.text.is_some() && valid_rect(span))
        .collect();
    runs.sort_by(|a, b| {
        let a_row = (a.y / pixel_height).floor();
        let b_row = (b.y / pixel_height).floor();
        a_row.total_cmp(&b_row).then_with(|| a.x.total_cmp(&b.x))
    });

    let tolerance = pixel_height / 2.0;
    let x_tolerance = f64::from(config.cell_width) * 3.0;
    let mut last_y: Option<f64> = None;
    let mut last_right_x: Option<f64> = None;
    let mut last_row: u16 = 0;
    let mut next_col: u16 = 0;

    for span in runs {
        let run_y = span.y;
        let indent = cell_at(span.x, config.cell_width);
        // CSS line heights are fractional: adjacent spans on the same visual
        // line can differ by a pixel or two and straddle a cell boundary.
        // Compare baselines with a tolerance, and require x continuity, or a
        // run positioned at a different column would be appended to the
        // previous line instead of starting at its own indent.
        let cell_contiguous =
            indent <= next_col.saturating_add(3) && next_col <= indent.saturating_add(3);
        // A CSS space is much narrower than a terminal cell, so a run of
        // " | " can advance `next_col` by three cells while its pixel edges
        // are exactly adjacent to the next word. Compare the document-space
        // right edge as well, otherwise the next word falls back to its own
        // indent and overwrites the tail of the previous word.
        let pixel_contiguous =
            last_right_x.is_some_and(|right| (span.x - right).abs() <= x_tolerance);
        let horizontally_contiguous = cell_contiguous || pixel_contiguous;
        let same_line = last_y.is_some_and(|previous| (run_y - previous).abs() < tolerance)
            && horizontally_contiguous;
        let mut col = if same_line { next_col } else { indent };
        let mut row = if same_line {
            last_row
        } else {
            cell_at(run_y, config.cell_height)
        };

        // A run whose target column is off the right edge (often a hidden
        // dropdown) must be skipped entirely, not placed one glyph and then
        // wrapped onto the following row.
        if col >= cols || row >= height {
            continue;
        }

        let style = style_for(span);
        let text = span.text.as_deref().unwrap_or_default();
        for grapheme in text.graphemes(true) {
            if grapheme == "\n" || grapheme == "\r" {
                row = row.saturating_add(1);
                col = indent;
                continue;
            }
            if col >= cols || row >= height {
                break;
            }
            if grapheme.chars().all(char::is_whitespace) {
                col = col.saturating_add(1);
                continue;
            }
            let used = surface.draw_grapheme_merged(col, row, grapheme, style);
            col = col.saturating_add(used.max(1));
            // Do not wrap a run's tail onto the next row: a word clipped by
            // the right edge is simply truncated, never leaked as a stray
            // character on the line below.
            if col >= cols {
                break;
            }
        }
        next_col = col;
        last_y = Some(run_y);
        last_right_x = Some(span.x + span.w);
        last_row = row;
    }

    // Images last: they occupy cells exclusively and carry PNG pixels. Runs
    // smaller than 2x2 cells are ignored completely (no reserve, no alt text).
    for span in spans
        .iter()
        .filter(|span| span.src.is_some() && valid_rect(span))
    {
        let x = cell_at(span.x, config.cell_width);
        let y = cell_at(span.y, config.cell_height);
        if x >= cols || y >= height {
            continue;
        }
        let width = cells_for(span.w, config.cell_width).min(cols - x);
        let image_height = cells_for(span.h, config.cell_height).min(height - y);
        if width < 2 || image_height < 2 {
            continue;
        }

        // Check for text *before* clearing, otherwise every reserved area
        // looks blank and an alt label would overwrite the surrounding words.
        let had_text = rect_has_text(&surface, x, y, width, image_height);
        surface.clear_rect(x, y, width, image_height);
        let alt = if had_text || width < 4 || image_height < 2 {
            String::new()
        } else {
            span.alt.clone().unwrap_or_default()
        };
        images.push(ImageRegion {
            x: span.x,
            y: span.y,
            w: span.w,
            h: span.h,
            cx: x,
            cy: y,
            cw: width,
            ch: image_height,
            alt,
        });
    }

    for span in spans
        .iter()
        .filter(|span| span.href.as_deref().is_some_and(|href| !href.is_empty()))
        .filter(|span| valid_rect(span))
    {
        if span.w > 0.0 && span.h > 0.0 {
            links.push(CdpLink {
                x: span.x,
                y: span.y,
                w: span.w,
                h: span.h,
                url: span.href.clone().unwrap_or_default(),
            });
        }
    }

    Painted {
        surface,
        links,
        images,
    }
}

/// Fill a reserved rectangle with an image's alt text when no PNG is available.
/// Only called for rectangles that were blank before reservation and are at
/// least 4x2 cells; guard here too so callers cannot accidentally overwrite
/// readable text later.
pub(crate) fn draw_alt(
    surface: &mut Surface,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    alt: &str,
    style: CellStyle,
) {
    if alt.is_empty() || width < 4 || height < 2 {
        return;
    }
    let last_row = y.saturating_add(height).min(surface.height);
    let last_col = x.saturating_add(width).min(surface.width);
    for (row, line) in (y..last_row).zip(alt.lines()) {
        let mut col = x;
        for grapheme in line.graphemes(true) {
            if col >= last_col {
                break;
            }
            if surface
                .get(col, row)
                .is_some_and(|cell| !cell.symbol.trim().is_empty())
            {
                col = col.saturating_add(1);
                continue;
            }
            let symbol = if grapheme.chars().any(char::is_control) {
                " "
            } else {
                grapheme
            };
            let used = surface.draw_grapheme(col, row, symbol, style);
            col = col.saturating_add(used.max(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, x: f64, y: f64, w: f64, h: f64) -> CdpSpan {
        CdpSpan {
            text: Some(text.to_owned()),
            src: None,
            alt: None,
            x,
            y,
            w,
            h,
            fg: None,
            bg: None,
            bold: None,
            italic: None,
            underline: None,
            href: None,
        }
    }

    fn image(x: f64, y: f64, w: f64, h: f64, alt: &str) -> CdpSpan {
        let mut span = span("", x, y, w, h);
        span.text = None;
        span.src = Some("https://example.com/pic.png".to_owned());
        span.alt = Some(alt.to_owned());
        span
    }

    fn row_text(surface: &Surface, row: u16) -> String {
        (0..surface.width)
            .filter_map(|col| surface.get(col, row))
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn cell_mapping_uses_floor_and_clamps() {
        assert_eq!(cell_at(0.0, 8), 0);
        assert_eq!(cell_at(7.9, 8), 0);
        assert_eq!(cell_at(8.0, 8), 1);
        assert_eq!(cell_at(24.0, 8), 3);
        assert_eq!(cell_at(f64::NAN, 8), 0);
        assert_eq!(cell_at(f64::INFINITY, 8), 0);
        assert_eq!(cells_for(17.0, 8), 3);
    }

    #[test]
    fn text_is_placed_one_grapheme_per_cell() {
        let config = CdpConfig::default();
        let spans = vec![span("abc", 8.0, 16.0, 24.0, 16.0)];
        let painted = paint_spans(&spans, &config, 10, 4);
        assert_eq!(
            painted.surface.get(1, 1).map(|c| c.symbol.as_str()),
            Some("a")
        );
        assert_eq!(
            painted.surface.get(2, 1).map(|c| c.symbol.as_str()),
            Some("b")
        );
        assert_eq!(
            painted.surface.get(3, 1).map(|c| c.symbol.as_str()),
            Some("c")
        );
        assert_eq!(
            painted.surface.get(4, 1).map(|c| c.symbol.as_str()),
            Some(" ")
        );
    }

    #[test]
    fn spans_are_sorted_by_visual_row_then_x() {
        let config = CdpConfig::default();
        let spans = vec![
            span("second", 40.0, 16.0, 48.0, 16.0),
            span("first", 0.0, 16.0, 40.0, 16.0),
            span("above", 0.0, 0.0, 40.0, 16.0),
        ];
        let painted = paint_spans(&spans, &config, 20, 4);
        assert_eq!(
            painted.surface.get(0, 0).map(|c| c.symbol.as_str()),
            Some("a")
        );
        assert_eq!(
            painted.surface.get(0, 1).map(|c| c.symbol.as_str()),
            Some("f")
        );
        assert_eq!(
            painted.surface.get(5, 1).map(|c| c.symbol.as_str()),
            Some("s")
        );
    }

    #[test]
    fn same_line_runs_with_close_baselines_continue_without_splitting() {
        let config = CdpConfig::default();
        // 15.9 and 16.1 straddle the cell boundary but are one visual line.
        let spans = vec![
            span("Thi", 0.0, 15.9, 24.0, 16.0),
            span(" s", 24.0, 16.1, 16.0, 16.0),
        ];
        let painted = paint_spans(&spans, &config, 20, 4);
        assert_eq!(row_text(&painted.surface, 0).trim_end(), "Thi s");
        assert_eq!(row_text(&painted.surface, 1).trim(), "");
    }

    #[test]
    fn narrow_css_spaces_do_not_truncate_following_words() {
        let config = CdpConfig::default();
        // CSS spaces are much narrower than a cell, so `next_col` drifts past
        // the next word's x. Pixel continuity must keep the words attached.
        let spans = vec![
            span("new", 122.0, 12.0, 24.0, 15.0),
            span(" | ", 147.0, 12.0, 11.0, 15.0),
            span("past", 158.0, 12.0, 25.0, 15.0),
            span(" | ", 183.5, 12.0, 10.9, 15.0),
            span("comments", 194.39, 12.0, 61.4, 15.0),
        ];
        let painted = paint_spans(&spans, &config, 60, 4);
        let row = row_text(&painted.surface, 0);
        assert!(row.contains("new | past | comments"), "{row:?}");
    }

    #[test]
    fn clipped_words_do_not_wrap_a_single_character_to_the_next_row() {
        let config = CdpConfig::default();
        let painted = paint_spans(&[span("login", 770.0, 0.0, 30.0, 15.0)], &config, 100, 3);
        assert_eq!(row_text(&painted.surface, 0).trim(), "logi");
        assert_eq!(row_text(&painted.surface, 1).trim(), "");
    }

    #[test]
    fn discontinuous_runs_start_at_their_own_indent() {
        let config = CdpConfig::default();
        let spans = vec![
            span("cont", 0.0, 16.0, 32.0, 16.0),
            span("Main", 400.0, 16.0, 32.0, 16.0),
        ];
        let painted = paint_spans(&spans, &config, 100, 4);
        let row = row_text(&painted.surface, 1);
        assert!(row.starts_with("cont"), "{row:?}");
        assert!(row[50..].starts_with("Main"), "{row:?}");
    }

    #[test]
    fn wide_grapheme_consumes_two_cells_and_bold_is_styled() {
        let config = CdpConfig::default();
        let mut wide = span("あ", 0.0, 0.0, 16.0, 16.0);
        wide.bold = Some(true);
        let painted = paint_spans(&[wide], &config, 10, 2);
        assert_eq!(
            painted.surface.get(0, 0).map(|c| c.symbol.as_str()),
            Some("あ")
        );
        assert!(painted.surface.get(0, 0).is_some_and(|c| c.style.bold));
        assert!(painted.surface.get(1, 0).is_some_and(|c| c.continuation));
    }

    #[test]
    fn off_edge_runs_are_skipped_entirely() {
        let config = CdpConfig::default();
        let spans = vec![span("hello", 72.0, 0.0, 40.0, 16.0)];
        let painted = paint_spans(&spans, &config, 8, 3);
        assert!(painted.surface.cells().iter().all(|c| c.symbol == " "));
    }

    #[test]
    fn huge_negative_and_non_finite_rects_are_ignored() {
        let config = CdpConfig::default();
        let spans = vec![
            span("bad", -8.0, 0.0, 24.0, 16.0),
            span("bad", f64::NAN, 0.0, 24.0, 16.0),
            span("bad", f64::INFINITY, 0.0, 24.0, 16.0),
            span("bad", 1e12, 0.0, 24.0, 16.0),
            span("bad", 0.0, 0.0, -24.0, 16.0),
        ];
        let painted = paint_spans(&spans, &config, 20, 3);
        assert!(painted.surface.cells().iter().all(|c| c.symbol == " "));
        assert!(painted.links.is_empty());
    }

    #[test]
    fn small_images_are_ignored_and_alt_never_overwrites_text() {
        let config = CdpConfig::default();
        let mut over_text = image(16.0, 16.0, 24.0, 32.0, "[image]");
        over_text.href = None;
        let spans = vec![
            span("Hacker News", 0.0, 16.0, 88.0, 16.0),
            over_text,
            image(0.0, 0.0, 8.0, 16.0, "tiny"),
        ];
        let painted = paint_spans(&spans, &config, 30, 6);
        assert_eq!(painted.images.len(), 1, "only the 3x2 image is reserved");
        assert!(
            painted.images[0].alt.is_empty(),
            "alt over real text must be suppressed"
        );
        let row = row_text(&painted.surface, 1);
        assert!(!row.contains("[image]"), "{row:?}");
        assert!(row.contains("News"), "{row:?}");
    }

    #[test]
    fn large_blank_image_may_keep_alt_text() {
        let config = CdpConfig::default();
        let mut painted = paint_spans(&[image(0.0, 0.0, 40.0, 40.0, "logo")], &config, 20, 6);
        assert_eq!(painted.images.len(), 1);
        assert_eq!(painted.images[0].alt, "logo");
        let region = painted.images.remove(0);
        draw_alt(
            &mut painted.surface,
            region.cx,
            region.cy,
            region.cw,
            region.ch,
            &region.alt,
            CellStyle::new(),
        );
        assert_eq!(row_text(&painted.surface, 0).trim_end(), "logo");
    }

    #[test]
    fn href_spans_produce_link_regions() {
        let config = CdpConfig::default();
        let mut link = span("go", 0.0, 0.0, 16.0, 16.0);
        link.href = Some("https://example.com/".to_owned());
        let painted = paint_spans(&[link], &config, 10, 2);
        assert_eq!(painted.links.len(), 1);
        assert_eq!(painted.links[0].url, "https://example.com/");
        assert_eq!((painted.links[0].x, painted.links[0].y), (0.0, 0.0));
    }

    #[test]
    fn surface_height_covers_document_and_caps_at_1024() {
        assert_eq!(surface_height(704.0, 24, 16), 44);
        assert_eq!(surface_height(64.0, 24, 16), 24);
        assert_eq!(surface_height(1_000_000.0, 24, 16), MAX_ROWS);
        assert_eq!(surface_height(f64::NAN, 24, 16), 24);
    }
}
