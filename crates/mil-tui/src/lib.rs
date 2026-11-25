//! Terminal UI for the `mil` browser.
//!
//! This crate is the composition layer between [`mil_paint::Surface`] and the
//! terminal. It owns:
//!
//! * the frame layout (tab bar, omnibox, document viewport, status bar),
//! * the conversion of painted cells into a ratatui [`Buffer`](ratatui::buffer::Buffer),
//! * inline images via [`ratatui_image`], with a per-image protocol cache, and
//! * the keyboard/mouse mapping from [`crossterm`] events to [`Action`]s.
//!
//! It deliberately knows nothing about tabs, networking or HTML: the caller
//! supplies a [`ViewState`] snapshot and consumes the [`Action`]s returned by
//! [`TerminalView::handle_event`].

#![forbid(unsafe_code)]

mod keys;
pub mod paint;
mod view;

pub use keys::{DEFAULT_BINDINGS, matches};
pub use paint::{render_surface, scroll_rows};
pub use view::TerminalView;

use std::collections::BTreeMap;

use mil_paint::Surface;

/// One entry in the tab bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    /// Human-readable page title.
    pub title: String,
    /// URL shown for the tab (typically the active page URL).
    pub url: String,
}

/// Everything the view needs to draw a single frame.
///
/// All fields are borrowed so the caller can build this cheaply from its own
/// application state. [`TerminalView::render`] keeps a copy of `url_input` so
/// that a later `Enter` in the omnibox can produce a complete
/// [`Action::Navigate`] even though [`TerminalView::handle_event`] only receives
/// an `editing_url` flag.
#[derive(Debug, Clone, Copy)]
pub struct ViewState<'a> {
    /// Tabs in display order.
    pub tabs: &'a [TabInfo],
    /// Index of the active tab in `tabs`.
    pub active: usize,
    /// Text currently in the omnibox (also used as the navigation target).
    pub url_input: &'a str,
    /// Whether the omnibox owns text input.
    pub editing_url: bool,
    /// One-line message shown in the status bar.
    pub status: &'a str,
    /// Page title shown in the status bar.
    pub title: &'a str,
    /// Whether to show a loading spinner.
    pub loading: bool,
    /// Painted document, if one is loaded.
    pub surface: Option<&'a Surface>,
    /// Vertical document scroll position, in cell rows (fractional).
    pub scroll_y: f64,
    /// Maximum meaningful value of `scroll_y`.
    pub scroll_max_y: f64,
}

/// A user intent produced by [`TerminalView::handle_event`].
///
/// The application is responsible for actually performing the action. The
/// three link actions are deliberately emitted by the view but implemented by
/// the app, because the view has no model of focusable links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Exit the browser.
    Quit,
    /// Navigate to the given URL.
    Navigate(String),
    /// Open a new tab.
    NewTab,
    /// Close the active tab.
    CloseTab,
    /// Switch to the next tab.
    NextTab,
    /// Switch to the previous tab.
    PrevTab,
    /// Reload the active page.
    Reload,
    /// History back.
    Back,
    /// History forward.
    Forward,
    /// Scroll the document by `i32` rows (positive is down).
    Scroll(i32),
    /// Jump to the top of the document.
    ScrollToTop,
    /// Jump to the bottom of the document.
    ScrollToBottom,
    /// Click at document-relative `(column, row)`.
    Click(u16, u16),
    /// Focus the omnibox.
    FocusUrl,
    /// Leave the omnibox.
    BlurUrl,
    /// Search for the given text.
    Find(String),
    /// Toggle the help overlay (rendered by the app).
    ToggleHelp,
    /// Bookmark the active page.
    Bookmark,
    /// Open the given URL in a new tab.
    OpenNewTab(String),
    /// Move link focus forward.
    NextLink,
    /// Move link focus backward.
    PrevLink,
    /// Activate the focused link.
    ActivateLink,
}

/// Re-export of the crossterm event type used by the public API.
pub use crossterm::event::Event as TerminalEvent;

/// Marker alias documenting the keybinding map shape.
///
/// Keys are canonical [`Action`] names (`"quit"`, `"scroll_down"`, …) and
/// values are key patterns such as `"q"`, `"ctrl-c"`, `"ctrl-shift-tab"`.
/// Several alternatives may be separated with `|`.
pub type Keymap = BTreeMap<String, String>;
