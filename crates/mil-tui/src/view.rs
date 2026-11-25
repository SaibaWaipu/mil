//! The terminal view: layout, image compositing and event translation.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use anyhow::Context;
use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use mil_paint::surface::{PlacedImage, Surface};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use ratatui_image::{
    Resize, StatefulImage,
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    Action, ViewState,
    keys::resolve,
    paint::{clamped_scroll_rows, clear_area, render_surface},
};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A cached, already-encoded image protocol.
struct CachedImage {
    protocol: StatefulProtocol,
    png: Arc<[u8]>,
}

/// Renders browser state to a ratatui terminal and translates input events.
///
/// Create this once per application (after entering the alternate screen), then
/// call [`render`](Self::render) for each frame and
/// [`handle_event`](Self::handle_event) for each input event.
pub struct TerminalView {
    picker: Picker,
    protocols: HashMap<u64, CachedImage>,
    viewport: Rect,
    /// Current omnibox text. Includes edits made by `handle_event`, which the
    /// application never sees as an `Action` and therefore cannot mirror back
    /// into `ViewState`.
    url_input: String,
    /// The `ViewState::url_input` last observed by `render`. A difference from
    /// it means the application deliberately changed the text (e.g. after a
    /// navigation) and our local edits must be discarded.
    url_baseline: String,
    /// First document row shown in the viewport, in cell rows.
    scroll_offset: u16,
    find_mode: bool,
    find_buffer: String,
    frame: u64,
}

impl TerminalView {
    /// Detect terminal graphics capabilities and create an empty view.
    ///
    /// Capability detection briefly reads/writes stdio and should be called
    /// after the alternate screen has been entered. If detection fails (for
    /// example in a CI runner), this falls back to ratatui-image's halfblocks
    /// protocol and [`image_support`](Self::image_support) returns `false`.
    pub fn new() -> anyhow::Result<Self> {
        // `MIL_IMAGE_PROTOCOL=none` (or `halfblocks`) skips the terminal
        // capability probe. Some terminals never answer those queries; the
        // probe's reader thread then lingers and can swallow subsequent
        // keystrokes. This is also useful for headless/CI runs.
        let skip_probe = std::env::var("MIL_IMAGE_PROTOCOL")
            .map(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "none" | "off" | "halfblocks"
                )
            })
            .unwrap_or(false);
        let picker = if skip_probe {
            Picker::halfblocks()
        } else {
            Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
        };
        Ok(Self {
            picker,
            protocols: HashMap::new(),
            viewport: Rect::ZERO,
            url_input: String::new(),
            url_baseline: String::new(),
            scroll_offset: 0,
            find_mode: false,
            find_buffer: String::new(),
            frame: 0,
        })
    }

    /// Whether a real graphics protocol (sixel, kitty or iTerm2) is available.
    ///
    /// When this is `false`, inline images are represented by their alt text.
    pub fn image_support(&self) -> bool {
        self.picker.protocol_type() != ProtocolType::Halfblocks
    }

    /// Draw one frame.
    pub fn render(
        &mut self,
        terminal: &mut DefaultTerminal,
        state: &ViewState<'_>,
    ) -> anyhow::Result<()> {
        self.sync_url_input(state);
        self.frame = self.frame.wrapping_add(1);
        terminal.draw(|frame| self.draw(frame, state))?;
        Ok(())
    }

    /// Adopt application-driven changes to the URL without losing characters
    /// typed into the omnibox while the application's snapshot is unchanged.
    fn sync_url_input(&mut self, state: &ViewState<'_>) {
        if !state.editing_url || state.url_input != self.url_baseline {
            self.url_input = state.url_input.to_owned();
        }
        self.url_baseline = state.url_input.to_owned();
    }

    /// Translate a crossterm event into an [`Action`].
    ///
    /// `editing_url` tells the view that the omnibox owns text input: in that
    /// mode printable characters and `Backspace` edit the remembered URL,
    /// `Enter` emits [`Action::Navigate`] with it, and `Escape` emits
    /// [`Action::BlurUrl`]. Pressing the default find binding (`/`) starts a
    /// small internal find buffer; `Enter` then emits [`Action::Find`] with the
    /// typed query and `Escape` cancels it. `keymap` overrides the canonical
    /// bindings listed in [`DEFAULT_BINDINGS`](crate::DEFAULT_BINDINGS).
    pub fn handle_event(
        &mut self,
        event: Event,
        editing_url: bool,
        keymap: &BTreeMap<String, String>,
    ) -> Option<Action> {
        match event {
            Event::Paste(text) => {
                // Paste belongs to whichever text field owns input; control
                // characters in a paste must never reach either buffer.
                let target = if self.find_mode {
                    Some(&mut self.find_buffer)
                } else if editing_url {
                    Some(&mut self.url_input)
                } else {
                    None
                };
                if let Some(target) = target {
                    target.extend(text.chars().filter(|ch| !ch.is_control()));
                }
                None
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    return None;
                }
                let modifiers = key.modifiers;
                let is_text = !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);

                if self.find_mode {
                    match key.code {
                        KeyCode::Char(c) if is_text => {
                            self.find_buffer.push(c);
                            return None;
                        }
                        KeyCode::Backspace if is_text => {
                            self.find_buffer.pop();
                            return None;
                        }
                        KeyCode::Enter => {
                            let query = std::mem::take(&mut self.find_buffer);
                            self.find_mode = false;
                            return Some(Action::Find(query));
                        }
                        KeyCode::Esc => {
                            self.find_mode = false;
                            self.find_buffer.clear();
                            return None;
                        }
                        _ => {}
                    }
                }

                if editing_url {
                    match key.code {
                        KeyCode::Char(c) if is_text => {
                            self.url_input.push(c);
                            return None;
                        }
                        KeyCode::Backspace if is_text => {
                            self.url_input.pop();
                            return None;
                        }
                        KeyCode::Enter => return Some(Action::Navigate(self.url_input.clone())),
                        KeyCode::Esc => return Some(Action::BlurUrl),
                        _ => {}
                    }
                }

                let action = resolve(&Event::Key(key), keymap)?;
                match action {
                    Action::Find(_) => {
                        self.find_mode = true;
                        self.find_buffer.clear();
                        None
                    }
                    other => Some(other),
                }
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>, state: &ViewState<'_>) {
        let area = frame.area();
        let (tabs, url, document, status) = if state.tabs.len() > 1 {
            let [tabs, url, document, status] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(area);
            (Some(tabs), url, document, status)
        } else {
            let [url, document, status] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(area);
            (None, url, document, status)
        };

        if let Some(tabs) = tabs {
            self.draw_tabs(frame, state, tabs);
        }
        self.draw_omnibox(frame, state, url);

        self.viewport = document;
        let offset = clamped_scroll_rows(state.scroll_y, state.scroll_max_y);
        self.scroll_offset = offset;
        {
            let buf = frame.buffer_mut();
            clear_area(buf, document);
            match state.surface {
                Some(surface) => render_surface(surface, buf, document, offset),
                None if document.height > 0 => {
                    buf.set_stringn(
                        document.x,
                        document.y,
                        "No page loaded - press Ctrl-L to enter a URL",
                        usize::from(document.width),
                        Style::new().fg(Color::DarkGray),
                    );
                }
                None => {}
            }
        }
        match state.surface {
            Some(surface) => self.draw_images(frame, surface, document, offset),
            // A page without a surface has no images: release the cached
            // protocols instead of keeping decoded PNGs alive indefinitely.
            None => self.protocols.clear(),
        }
        self.draw_status(frame, state, status);
    }

    fn draw_tabs(&self, frame: &mut Frame<'_>, state: &ViewState<'_>, area: Rect) {
        if area.is_empty() {
            return;
        }
        let mut spans = Vec::with_capacity(state.tabs.len() * 2);
        for (index, tab) in state.tabs.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" | ", Style::new().fg(Color::DarkGray)));
            }
            let label = truncate(&tab.title, 28);
            let style = if index == state.active {
                Style::new()
                    .fg(Color::Cyan)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            spans.push(Span::styled(format!(" {label} "), style));
        }
        clear_area(frame.buffer_mut(), area);
        frame.render_widget(Line::from(spans), area);
    }

    fn draw_omnibox(&self, frame: &mut Frame<'_>, state: &ViewState<'_>, area: Rect) {
        if area.is_empty() {
            return;
        }
        let (prefix, text_style) = if state.editing_url {
            (
                " URL ",
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            )
        } else {
            (" url ", Style::new().fg(Color::Reset))
        };
        let mut spans = vec![Span::styled(
            prefix,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )];
        // Draw the cached buffer, which includes characters typed locally
        // while the application's `ViewState` snapshot is unchanged.
        spans.push(Span::styled(&self.url_input, text_style));
        if state.editing_url {
            spans.push(Span::styled(
                "█",
                Style::new().add_modifier(Modifier::REVERSED),
            ));
        }
        clear_area(frame.buffer_mut(), area);
        frame.render_widget(Line::from(spans), area);
    }

    fn draw_status(&self, frame: &mut Frame<'_>, state: &ViewState<'_>, area: Rect) {
        // Ratatui's `set_stringn` indexes the start cell without checking the
        // buffer, so a zero-height status row (tiny terminal) must be skipped.
        if area.is_empty() {
            return;
        }
        let spinner = if state.loading {
            SPINNER[(self.frame as usize / 2) % SPINNER.len()]
        } else {
            ""
        };
        let percent = if state.scroll_max_y <= 0.0 || !state.scroll_max_y.is_finite() {
            100
        } else {
            (state.scroll_y.clamp(0.0, state.scroll_max_y) / state.scroll_max_y * 100.0)
                .clamp(0.0, 100.0)
                .round() as u16
        };
        // The find state lives in the view, so show its prompt here; the
        // application only learns the finished query via `Action::Find`.
        let left = if self.find_mode {
            format!(" /{}█", self.find_buffer)
        } else if state.status.is_empty() {
            format!(" {} {}", state.title, spinner)
        } else {
            format!(" {} {} - {}", state.title, spinner, state.status)
        };
        let right = format!("{percent:>3}%  ? help  ^L url  q quit ");
        let right_width = right.width() as u16;
        let left_width = area.width.saturating_sub(right_width);

        let buf = frame.buffer_mut();
        clear_area(buf, area);
        buf.set_stringn(
            area.x,
            area.y,
            truncate(&left, usize::from(left_width)),
            usize::from(left_width),
            Style::new().fg(Color::Gray),
        );
        let right_x = area.x.saturating_add(left_width);
        buf.set_stringn(
            right_x,
            area.y,
            &right,
            usize::from(area.right().saturating_sub(right_x)),
            Style::new().fg(Color::DarkGray),
        );
    }

    fn draw_images(
        &mut self,
        frame: &mut Frame<'_>,
        surface: &Surface,
        document: Rect,
        offset: u16,
    ) {
        let ids: HashSet<u64> = surface.images().iter().map(|image| image.id).collect();
        self.protocols.retain(|id, _| ids.contains(id));

        let mut seen_ids = HashSet::new();
        for image in surface.images() {
            let Some(rect) = image_rect(image, document, offset) else {
                continue;
            };
            // The protocol cache is keyed by id. If the surface contains two
            // different images with the same id, rendering both from one
            // cached protocol would show the wrong picture for one of them.
            if !seen_ids.insert(image.id) || !self.image_support() || !self.ensure_protocol(image) {
                self.draw_alt(frame, image, rect);
                continue;
            }
            let Some(cached) = self.protocols.get_mut(&image.id) else {
                continue;
            };
            frame.render_stateful_widget(
                StatefulImage::new().resize(Resize::Scale(None)),
                rect,
                &mut cached.protocol,
            );
            // Drain the latest encode result; errors are non-fatal for a frame.
            let _ = cached.protocol.last_encoding_result();
        }
    }

    /// Return true when a protocol for this image is ready to render.
    fn ensure_protocol(&mut self, placed: &PlacedImage) -> bool {
        if let Some(cached) = self.protocols.get(&placed.id)
            && cached.png.as_ref() == placed.png.as_ref()
        {
            return true;
        }
        let Ok(decoded) = image::load_from_memory(&placed.png).context("decode inline PNG") else {
            return false;
        };
        let protocol = self.picker.new_resize_protocol(decoded);
        self.protocols.insert(
            placed.id,
            CachedImage {
                protocol,
                png: Arc::clone(&placed.png),
            },
        );
        true
    }

    fn draw_alt(&self, frame: &mut Frame<'_>, image: &PlacedImage, rect: Rect) {
        let label = if image.alt.is_empty() {
            "[image]".to_owned()
        } else {
            image.alt.clone()
        };
        clear_area(frame.buffer_mut(), rect);
        frame.render_widget(
            Paragraph::new(label.as_str())
                .style(Style::new().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            rect,
        );
    }

    fn handle_mouse(&self, mouse: MouseEvent) -> Option<Action> {
        match mouse.kind {
            MouseEventKind::ScrollUp => Some(Action::Scroll(-3)),
            MouseEventKind::ScrollDown => Some(Action::Scroll(3)),
            MouseEventKind::Down(MouseButton::Left) => {
                if !self
                    .viewport
                    .contains(Position::new(mouse.column, mouse.row))
                {
                    return None;
                }
                // `Action::Click` is document-relative: undo the viewport
                // offset and add back the rows scrolled off the top.
                Some(Action::Click(
                    mouse.column.saturating_sub(self.viewport.x),
                    mouse
                        .row
                        .saturating_sub(self.viewport.y)
                        .saturating_add(self.scroll_offset),
                ))
            }
            _ => None,
        }
    }
}

/// Screen rectangle of an image intersecting the document viewport.
fn image_rect(image: &PlacedImage, document: Rect, offset: u16) -> Option<Rect> {
    let left = i32::from(image.x) + i32::from(document.x);
    let top = i32::from(image.y) + i32::from(document.y) - i32::from(offset);
    let right = left + i32::from(image.width);
    let bottom = top + i32::from(image.height);
    let view_left = i32::from(document.x);
    let view_top = i32::from(document.y);
    let view_right = view_left + i32::from(document.width);
    let view_bottom = view_top + i32::from(document.height);

    let x = left.max(view_left);
    let y = top.max(view_top);
    let r = right.min(view_right);
    let b = bottom.min(view_bottom);
    if r <= x || b <= y {
        return None;
    }
    Some(Rect::new(
        x as u16,
        y as u16,
        (r - x) as u16,
        (b - y) as u16,
    ))
}

fn truncate(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_owned();
    }
    let mut result = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width >= max_width {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::picker::Picker;

    use crate::TabInfo;

    use super::*;

    fn view() -> TerminalView {
        TerminalView {
            picker: Picker::halfblocks(),
            protocols: HashMap::new(),
            viewport: Rect::new(0, 2, 40, 10),
            url_input: String::new(),
            url_baseline: String::new(),
            scroll_offset: 0,
            find_mode: false,
            find_buffer: String::new(),
            frame: 0,
        }
    }

    fn state<'a>(
        url_input: &'a str,
        editing_url: bool,
        tabs: &'a [TabInfo],
        surface: Option<&'a Surface>,
    ) -> ViewState<'a> {
        ViewState {
            tabs,
            active: 0,
            url_input,
            editing_url,
            status: "",
            title: "title",
            loading: false,
            surface,
            scroll_y: 0.0,
            scroll_max_y: 0.0,
        }
    }

    fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn omnibox_consumes_text_and_navigates_on_enter() {
        let mut view = view();
        let empty = BTreeMap::new();
        view.url_input = "https://example.org".to_owned();
        assert_eq!(
            view.handle_event(key(KeyCode::Char('x'), KeyModifiers::NONE), true, &empty),
            None
        );
        assert_eq!(view.url_input, "https://example.orgx");
        assert_eq!(
            view.handle_event(key(KeyCode::Enter, KeyModifiers::NONE), true, &empty),
            Some(Action::Navigate("https://example.orgx".to_owned()))
        );
        assert_eq!(
            view.handle_event(key(KeyCode::Esc, KeyModifiers::NONE), true, &empty),
            Some(Action::BlurUrl)
        );
        assert_eq!(
            view.handle_event(key(KeyCode::Char('q'), KeyModifiers::NONE), true, &empty),
            None
        );
    }

    #[test]
    fn find_mode_buffers_query() {
        let mut view = view();
        let empty = BTreeMap::new();
        assert_eq!(
            view.handle_event(key(KeyCode::Char('/'), KeyModifiers::NONE), false, &empty),
            None
        );
        for ch in "test".chars() {
            assert_eq!(
                view.handle_event(key(KeyCode::Char(ch), KeyModifiers::NONE), false, &empty),
                None
            );
        }
        assert_eq!(
            view.handle_event(key(KeyCode::Enter, KeyModifiers::NONE), false, &empty),
            Some(Action::Find("test".to_owned()))
        );
    }

    #[test]
    fn keymap_override_and_scroll_bindings() {
        let mut view = view();
        let empty = BTreeMap::new();
        assert_eq!(
            view.handle_event(key(KeyCode::Char('j'), KeyModifiers::NONE), false, &empty),
            Some(Action::Scroll(1))
        );
        let mut map = BTreeMap::new();
        map.insert("quit".to_owned(), "x".to_owned());
        assert_eq!(
            view.handle_event(key(KeyCode::Char('x'), KeyModifiers::NONE), false, &map),
            Some(Action::Quit)
        );
        assert_eq!(
            view.handle_event(key(KeyCode::Char('q'), KeyModifiers::NONE), false, &map),
            None
        );
    }

    #[test]
    fn mouse_events_are_translated_relative_to_viewport() {
        let mut view = view();
        let empty = BTreeMap::new();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            view.handle_event(Event::Mouse(click), false, &empty),
            Some(Action::Click(5, 2))
        );
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            view.handle_event(Event::Mouse(wheel), false, &empty),
            Some(Action::Scroll(3))
        );
        let outside = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            view.handle_event(Event::Mouse(outside), false, &empty),
            None
        );
    }

    #[test]
    fn image_rect_clips_scrolling_and_viewport() {
        let doc = Rect::new(0, 2, 10, 5);
        let image = PlacedImage::new(1, 2, 4, 6, 4, vec![], "alt");
        assert_eq!(image_rect(&image, doc, 0), Some(Rect::new(2, 6, 6, 1)));
        assert_eq!(image_rect(&image, doc, 3), Some(Rect::new(2, 3, 6, 4)));
        assert_eq!(image_rect(&image, doc, 20), None);

        let wide = PlacedImage::new(2, 8, 3, 6, 2, vec![], "wide");
        assert_eq!(image_rect(&wide, doc, 0), Some(Rect::new(8, 5, 2, 2)));
    }

    #[test]
    fn sync_url_input_keeps_local_edits_until_the_app_changes_text() {
        let mut view = view();
        view.url_input = "example.org".to_owned();
        view.url_baseline = "example.org".to_owned();
        view.sync_url_input(&state("example.org", true, &[], None));
        assert_eq!(view.url_input, "example.org");

        let app_change = "https://example.org".to_owned();
        view.sync_url_input(&state(&app_change, true, &[], None));
        assert_eq!(view.url_input, app_change);
        assert_eq!(view.url_baseline, app_change);

        // Losing focus discards an unsubmitted local edit.
        view.url_input.push_str("/draft");
        view.sync_url_input(&state(&app_change, false, &[], None));
        assert_eq!(view.url_input, app_change);
    }

    #[test]
    fn locally_typed_url_is_visible_in_the_omnibox() {
        let mut view = view();
        let empty = BTreeMap::new();
        view.handle_event(key(KeyCode::Char('e'), KeyModifiers::NONE), true, &empty);
        let state = state("", true, &[], None);
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        terminal.draw(|frame| view.draw(frame, &state)).unwrap();
        assert!(row(&terminal, 1).contains('e'));
    }

    #[test]
    fn find_mode_state_machine_handles_editing_cancel_and_escape_hatches() {
        let mut view = view();
        let empty = BTreeMap::new();
        assert_eq!(
            view.handle_event(key(KeyCode::Char('/'), KeyModifiers::NONE), false, &empty),
            None
        );
        assert!(view.find_mode);
        assert_eq!(
            view.handle_event(key(KeyCode::Char('a'), KeyModifiers::NONE), false, &empty),
            None
        );
        assert_eq!(
            view.handle_event(key(KeyCode::Char('b'), KeyModifiers::NONE), false, &empty),
            None
        );
        assert_eq!(
            view.handle_event(key(KeyCode::Backspace, KeyModifiers::NONE), false, &empty),
            None
        );
        assert_eq!(view.find_buffer, "a");
        // A plain letter is query text, not the `quit` binding.
        assert_eq!(
            view.handle_event(key(KeyCode::Char('q'), KeyModifiers::NONE), false, &empty),
            None
        );
        assert_eq!(view.find_buffer, "aq");
        assert_eq!(
            view.handle_event(Event::Paste("xy\nz".to_owned()), false, &empty),
            None
        );
        assert_eq!(view.find_buffer, "aqxyz");
        // Explicit keybindings still work while the find prompt is open.
        assert_eq!(
            view.handle_event(
                key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false,
                &empty
            ),
            Some(Action::Quit)
        );
        assert_eq!(
            view.handle_event(key(KeyCode::Esc, KeyModifiers::NONE), false, &empty),
            None
        );
        assert!(!view.find_mode);
        assert!(view.find_buffer.is_empty());

        // A fresh `/` clears the previous query.
        view.find_buffer = "stale".to_owned();
        view.handle_event(key(KeyCode::Char('/'), KeyModifiers::NONE), false, &empty);
        assert!(view.find_buffer.is_empty());
        assert_eq!(
            view.handle_event(key(KeyCode::Enter, KeyModifiers::NONE), false, &empty),
            Some(Action::Find(String::new()))
        );
        assert!(!view.find_mode);
    }

    #[test]
    fn find_prompt_is_rendered_while_collecting_a_query() {
        let mut view = view();
        let empty = BTreeMap::new();
        view.handle_event(key(KeyCode::Char('/'), KeyModifiers::NONE), false, &empty);
        for ch in "abc".chars() {
            view.handle_event(key(KeyCode::Char(ch), KeyModifiers::NONE), false, &empty);
        }
        let state = state("", false, &[], None);
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        terminal.draw(|frame| view.draw(frame, &state)).unwrap();
        assert!(row(&terminal, 3).contains("/abc"));
    }

    #[test]
    fn clicks_are_relative_to_the_scrolled_document() {
        let mut view = view();
        view.scroll_offset = 5;
        let empty = BTreeMap::new();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            view.handle_event(Event::Mouse(click), false, &empty),
            Some(Action::Click(5, 7))
        );
    }

    #[test]
    fn draw_clamps_scroll_and_survives_tiny_terminals() {
        for (width, height) in [(40, 5), (4, 1), (0, 0)] {
            let mut view = view();
            let mut state = state("", true, &[], None);
            state.scroll_y = 100.0;
            state.scroll_max_y = 2.0;
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| view.draw(frame, &state)).unwrap();
            assert_eq!(view.scroll_offset, 2);
        }
    }
}
