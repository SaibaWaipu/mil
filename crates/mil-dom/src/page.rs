//! A parsed web page and the terminal-oriented views of its DOM.

use std::collections::HashMap;

use anyhow::Result;
use blitz_dom::{BaseDocument, DocumentConfig, Node, local_name, util::ToColorColor};
use blitz_html::HtmlDocument;
use blitz_traits::events::HitResult;
use blitz_traits::shell::Viewport;
use mil_paint::Rgb;
use style::values::computed::{FontStyle, TextDecorationLine};
use url::Url;

/// `blitz-html` reports parse errors with `println!`, which would paint
/// diagnostics straight into the alternate screen. Redirect stdout to
/// `/dev/null` for the duration of parsing (single-threaded by design).
#[cfg(unix)]
struct SilenceStdout {
    saved: std::os::fd::RawFd,
    devnull: std::os::fd::RawFd,
}

#[cfg(unix)]
impl SilenceStdout {
    fn new() -> Self {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        unsafe {
            let saved = libc::dup(1);
            let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if saved >= 0 && devnull >= 0 {
                libc::dup2(devnull, 1);
            }
            Self { saved, devnull }
        }
    }
}

#[cfg(unix)]
impl Drop for SilenceStdout {
    fn drop(&mut self) {
        unsafe {
            if self.saved >= 0 {
                libc::dup2(self.saved, 1);
                libc::close(self.saved);
            }
            if self.devnull >= 0 {
                libc::close(self.devnull);
            }
        }
    }
}

#[cfg(not(unix))]
struct SilenceStdout;
#[cfg(not(unix))]
impl SilenceStdout {
    fn new() -> Self {
        Self
    }
}

/// A parsed HTML document, its URL and its title.
pub struct Page {
    /// The underlying Blitz document. Styles and layout are resolved by
    /// [`Page::resolve`].
    pub doc: BaseDocument,
    /// The URL the document was parsed from; used to resolve relative links.
    pub url: Url,
    /// The document title, or an empty string when there is no `<title>`.
    pub title: String,
}

impl Page {
    /// Parse `html` into a page rooted at `url`, using `viewport` for layout.
    pub fn from_html(html: &str, url: &Url, viewport: Viewport) -> Result<Self> {
        let _quiet = SilenceStdout::new();
        let config = DocumentConfig {
            viewport: Some(viewport),
            base_url: Some(url.to_string()),
            ..DocumentConfig::default()
        };
        let doc = HtmlDocument::from_html(html, config).into_inner();
        let title = doc
            .find_title_node()
            .map(Node::text_content)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_default();
        Ok(Self {
            doc,
            url: url.clone(),
            title,
        })
    }

    /// Restyle and relayout the document. `now` is the animation clock in
    /// seconds.
    pub fn resolve(&mut self, now: f64) {
        let _quiet = SilenceStdout::new();
        self.doc.resolve(now);
    }

    /// Attach a fetched author stylesheet to its `<link>` element.
    pub fn add_stylesheet(&mut self, node_id: usize, css: &str) -> Result<()> {
        let _quiet = SilenceStdout::new();
        let stylesheet = self
            .doc
            .make_stylesheet(css, style::stylesheets::origin::Origin::Author);
        self.doc.add_stylesheet_for_node(stylesheet, node_id);
        Ok(())
    }

    /// Document title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Re-read the title from the live document after a DOM mutation.
    pub fn refresh_title(&mut self) {
        self.title = self
            .doc
            .find_title_node()
            .map(Node::text_content)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_default();
    }

    /// Text recovered from resolved inline layouts, in pixel coordinates
    /// relative to the document origin.
    pub fn text_runs(&self) -> Vec<TextRun> {
        self.collect().0
    }

    /// Element background colours with their absolute pixel rectangles.
    pub fn backgrounds(&self) -> Vec<FillRect> {
        let mut rects = Vec::new();
        for (_, node) in self.doc.tree().iter() {
            if !node.is_element() {
                continue;
            }
            let Some(styles) = node.primary_styles() else {
                continue;
            };
            let current_color = styles.clone_color();
            let background = styles
                .clone_background_color()
                .resolve_to_absolute(&current_color)
                .as_color_color();
            if background.components[3] <= f32::EPSILON {
                continue;
            }
            let size = node.final_layout.size;
            if size.width <= 0.0 || size.height <= 0.0 {
                continue;
            }
            let position = node.absolute_position(0.0, 0.0);
            rects.push(FillRect {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
                color: to_rgb(background.components),
            });
        }
        rects
    }

    /// Hyperlink regions with absolute URLs, derived from the text runs each
    /// `<a href>` contains.
    pub fn links(&self) -> Vec<LinkRegion> {
        self.collect().1
    }

    /// Hit test in document pixel coordinates.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<HitResult> {
        self.doc.hit(x, y)
    }

    /// Collect text runs and link regions in one DOM traversal.
    fn collect(&self) -> (Vec<TextRun>, Vec<LinkRegion>) {
        let doc = &self.doc;
        let scroll = doc.viewport_scroll();
        let mut raw: Vec<RawRun> = Vec::new();

        for (id, node) in doc.tree().iter() {
            if is_non_rendering(doc, id) {
                continue;
            }
            let Some(text_layout) = node
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                continue;
            };
            let layout = &text_layout.layout;
            let scale = layout.scale().max(f32::EPSILON);

            // Inline layout coordinates begin at the content box of the inline
            // root. The viewport scroll is applied at the document root.
            let origin = node.absolute_position(
                node.final_layout.padding.left + node.final_layout.border.left - scroll.x as f32,
                node.final_layout.padding.top + node.final_layout.border.top - scroll.y as f32,
            );

            // A positioned GlyphRun is one style span of its underlying
            // parley Run. Comparing `Cluster::first_style()` to
            // `GlyphRun::style()` is not sufficient: separate spans with the
            // same style both match every cluster. Consume each Run's clusters
            // in visual order instead, advancing a cursor by the number of
            // glyphs in each positioned span so every cluster is emitted once.
            let mut cluster_cursors: HashMap<(usize, usize), usize> = HashMap::new();
            for (line_index, line) in layout.lines().enumerate() {
                for item in line.items() {
                    let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                        continue;
                    };
                    let run = glyph_run.run();
                    let glyph_count = glyph_run.glyphs().count();
                    if glyph_count == 0 {
                        continue;
                    }
                    let cursor = cluster_cursors
                        .entry((line_index, run.index()))
                        .or_insert(0);
                    let mut remaining = glyph_count;
                    let mut text = String::new();
                    for cluster in run.visual_clusters().skip(*cursor) {
                        let cluster_glyphs = cluster.glyphs().count();
                        if let Some(cluster_text) = text_layout.text.get(cluster.text_range()) {
                            text.push_str(cluster_text);
                        }
                        *cursor += 1;
                        if cluster_glyphs >= remaining {
                            break;
                        }
                        remaining -= cluster_glyphs;
                    }
                    if text.is_empty() {
                        continue;
                    }
                    let node_id = glyph_run.style().brush.id;
                    let font_size = run.font_size() / scale;
                    let style = doc
                        .get_node(node_id)
                        .and_then(|node| text_style(node, font_size))
                        .unwrap_or_else(|| TextStyle {
                            font_size,
                            ..TextStyle::default()
                        });
                    raw.push(RawRun {
                        text: text.to_owned(),
                        x: origin.x + glyph_run.offset() / scale,
                        y: origin.y + (glyph_run.baseline() - run.font_size()) / scale,
                        width: glyph_run.advance() / scale,
                        height: font_size,
                        style,
                        node_id,
                        link_node: find_link_ancestor(doc, node_id),
                    });
                }
            }
        }

        let mut anchor_ids: Vec<usize> = doc
            .query_selector_all("a[href]")
            .unwrap_or_default()
            .into_iter()
            .collect();
        for run in &raw {
            if let Some(id) = run.link_node {
                if !anchor_ids.contains(&id) {
                    anchor_ids.push(id);
                }
            }
        }

        let mut links = Vec::new();
        let mut link_indices = HashMap::new();
        for anchor_id in anchor_ids {
            if is_non_rendering(doc, anchor_id) {
                continue;
            }
            let Some(node) = doc.get_node(anchor_id) else {
                continue;
            };
            let Some(href) = node.attr(local_name!("href")) else {
                continue;
            };
            let Ok(url) = self.url.join(href) else {
                continue;
            };
            let mut bounds = [
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ];
            for run in &raw {
                if run.link_node != Some(anchor_id) {
                    continue;
                }
                bounds[0] = bounds[0].min(run.x);
                bounds[1] = bounds[1].min(run.y);
                bounds[2] = bounds[2].max(run.x + run.width);
                bounds[3] = bounds[3].max(run.y + run.height);
            }
            if !bounds[0].is_finite() {
                continue;
            }
            let index = links.len();
            link_indices.insert(anchor_id, index);
            links.push(LinkRegion {
                index,
                url: url.to_string(),
                x: bounds[0],
                y: bounds[1],
                width: (bounds[2] - bounds[0]).max(0.0),
                height: (bounds[3] - bounds[1]).max(0.0),
                node_id: anchor_id,
            });
        }

        let runs = raw
            .into_iter()
            .map(|run| TextRun {
                text: run.text,
                x: run.x,
                y: run.y,
                width: run.width,
                height: run.height,
                style: run.style,
                node_id: run.node_id,
                link_index: run.link_node.and_then(|id| link_indices.get(&id).copied()),
            })
            .collect();
        (runs, links)
    }
}

/// Internal text run carrying the id of the enclosing `<a>` element.
struct RawRun {
    text: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    style: TextStyle,
    node_id: usize,
    link_node: Option<usize>,
}

/// A run of source text with its resolved layout rectangle.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub style: TextStyle,
    /// Id of the node whose computed style produced this run.
    pub node_id: usize,
    /// Index into [`Page::links`] when the run is inside a hyperlink.
    pub link_index: Option<usize>,
}

/// Terminal-representable attributes of a text run.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TextStyle {
    pub color: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub font_size: f32,
}

/// A solid-colour element background in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FillRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: Rgb,
}

/// An `<a href>` region in pixel coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRegion {
    pub index: usize,
    pub url: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Id of the `<a>` element in the Blitz DOM.
    pub node_id: usize,
}

/// Elements whose contents must never appear as terminal text or links.
/// Scripts/styles are hidden by the UA stylesheet, and `noscript` is noise
/// for a client that can execute at least some JavaScript.
fn is_non_rendering(doc: &BaseDocument, mut id: usize) -> bool {
    loop {
        let Some(node) = doc.get_node(id) else {
            return true;
        };
        if let Some(element) = node.element_data() {
            let name = &element.name.local;
            if *name == local_name!("script")
                || *name == local_name!("style")
                || *name == local_name!("noscript")
                || *name == local_name!("template")
                || *name == local_name!("head")
            {
                return true;
            }
        }
        match node.parent {
            Some(parent) => id = parent,
            None => return false,
        }
    }
}

fn text_style(node: &Node, font_size: f32) -> Option<TextStyle> {
    let styles = node.primary_styles()?;
    let color = styles
        .get_inherited_text()
        .color
        .as_color_color()
        .components;
    let font = styles.get_font();
    let font_style = font.font_style;
    let italic = if font_style == FontStyle::ITALIC {
        true
    } else if font_style == FontStyle::NORMAL {
        false
    } else {
        font_style.oblique_degrees().abs() > 0.0
    };
    let underline = styles
        .get_text()
        .text_decoration_line
        .contains(TextDecorationLine::UNDERLINE);
    Some(TextStyle {
        color: Some(to_rgb(color)),
        bold: font.font_weight.is_bold(),
        italic,
        underline,
        font_size,
    })
}

fn find_link_ancestor(doc: &BaseDocument, mut id: usize) -> Option<usize> {
    loop {
        let node = doc.get_node(id)?;
        if node
            .element_data()
            .is_some_and(|element| element.name.local == local_name!("a"))
            && node.attr(local_name!("href")).is_some()
        {
            return Some(id);
        }
        id = node.layout_parent.get()?;
    }
}

fn to_rgb(components: [f32; 4]) -> Rgb {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb::new(
        byte(components[0]),
        byte(components[1]),
        byte(components[2]),
    )
}
