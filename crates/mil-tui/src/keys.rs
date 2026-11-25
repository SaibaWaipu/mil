//! Key-pattern parsing and the default keymap.
//!
//! A pattern is a sequence of optional modifiers followed by a key name:
//! `ctrl-c`, `alt-left`, `ctrl-shift-tab`, `shift-g`, `pgdn`, `q`. Alternatives
//! can be combined with `|` in keymap values, e.g. `"j|down"`.

use std::collections::BTreeMap;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::Action;

/// Default bindings, in resolution priority order.
///
/// The map passed to [`TerminalView::handle_event`](crate::TerminalView::handle_event)
/// overrides any entry by canonical action name. Entries that are present in
/// the map but unknown here are ignored.
pub const DEFAULT_BINDINGS: &[(&str, &[&str])] = &[
    ("quit", &["q", "ctrl-c", "ctrl-q"]),
    ("focus_url", &["ctrl-l"]),
    ("blur_url", &["esc"]),
    ("new_tab", &["ctrl-t"]),
    ("close_tab", &["ctrl-w"]),
    ("next_tab", &["ctrl-tab"]),
    ("prev_tab", &["ctrl-shift-tab"]),
    ("find", &["/"]),
    ("reload", &["r"]),
    ("back", &["alt-left", "backspace"]),
    ("forward", &["alt-right"]),
    ("scroll_page_down", &["ctrl-d", "ctrl-f", "pgdn"]),
    ("scroll_page_up", &["ctrl-u", "ctrl-b", "pgup"]),
    // `shift-g` must precede `g`: plain characters match case-insensitively.
    ("scroll_bottom", &["shift-g"]),
    ("scroll_top", &["g"]),
    ("scroll_down", &["j", "down"]),
    ("scroll_up", &["k", "up"]),
    ("next_link", &["tab"]),
    ("prev_link", &["shift-tab"]),
    ("activate_link", &["enter"]),
    ("toggle_help", &["?"]),
    ("bookmark", &["b"]),
];

/// Returns true when `event` is a key press matching `key_pattern`.
///
/// Modifier prefixes are `ctrl-`, `alt-` and `shift-` (in any order). Named
/// keys include `enter`, `esc`, `tab`, `backtab`, `backspace`, `delete`,
/// `space`, `up`, `down`, `left`, `right`, `pgup`, `pgdn`, `home`, `end` and
/// `f1`…`f12`. A single lowercase character matches case-insensitively, except
/// that an explicit uppercase character (`"G"`) only matches an uppercase key.
///
/// `shift-tab` additionally matches crossterm's `BackTab` key code, which is
/// what most terminals emit with or without the SHIFT modifier bit.
pub fn matches(key_pattern: &str, event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    if key.kind == KeyEventKind::Release {
        return false;
    }

    let mut rest = key_pattern.trim();
    let mut want_ctrl = false;
    let mut want_alt = false;
    let mut want_shift = false;
    // Modifiers may appear in any order, including repeats (e.g.
    // `alt-ctrl-alt-c`). Parse until no prefix matches instead of making one
    // pass per modifier, which would fail on `alt-ctrl-c`.
    loop {
        if strip_prefix_ci("ctrl-", &mut rest) {
            want_ctrl = true;
            continue;
        }
        if strip_prefix_ci("alt-", &mut rest) {
            want_alt = true;
            continue;
        }
        if strip_prefix_ci("shift-", &mut rest) {
            want_shift = true;
            continue;
        }
        break;
    }
    if rest.is_empty() {
        return false;
    }
    let Some(expected) = key_code(rest) else {
        return false;
    };

    let pressed_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let pressed_alt = key.modifiers.contains(KeyModifiers::ALT);
    let pressed_shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if pressed_ctrl != want_ctrl || pressed_alt != want_alt {
        return false;
    }
    if want_shift && !pressed_shift && !is_upper_char(key.code) {
        // `shift-tab` is commonly reported as BackTab without SHIFT.
        let shift_tab = rest.eq_ignore_ascii_case("tab") && key.code == KeyCode::BackTab;
        if !shift_tab {
            return false;
        }
    }
    // An unrequested SHIFT is harmless for character keys (terminals disagree
    // about whether uppercase characters set the bit), but named keys should
    // not fire when an extra modifier is held.
    if !want_shift && pressed_shift && !matches!(expected, KeyCode::Char(_) | KeyCode::BackTab) {
        return false;
    }

    match expected {
        KeyCode::Tab if want_shift => {
            key.code == KeyCode::BackTab || (key.code == KeyCode::Tab && pressed_shift)
        }
        expected => codes_equal(expected, key.code),
    }
}

/// Resolve a single event to an [`Action`] using overrides, then defaults.
pub(crate) fn resolve(event: &Event, keymap: &BTreeMap<String, String>) -> Option<Action> {
    for (name, defaults) in DEFAULT_BINDINGS {
        let matched = match lookup(keymap, name) {
            Some(patterns) => patterns.split('|').any(|p| matches(p.trim(), event)),
            None => defaults.iter().any(|p| matches(p, event)),
        };
        if matched {
            return action_for(name);
        }
    }
    None
}

fn lookup<'a>(keymap: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    if let Some(value) = keymap.get(name) {
        return Some(value.as_str());
    }
    // Accept `scroll-down` as an alias for `scroll_down`.
    keymap.get(&name.replace('_', "-")).map(String::as_str)
}

fn action_for(name: &str) -> Option<Action> {
    Some(match name {
        "quit" => Action::Quit,
        "focus_url" => Action::FocusUrl,
        "blur_url" => Action::BlurUrl,
        "new_tab" => Action::NewTab,
        "close_tab" => Action::CloseTab,
        "next_tab" => Action::NextTab,
        "prev_tab" => Action::PrevTab,
        "find" => Action::Find(String::new()),
        "reload" => Action::Reload,
        "back" => Action::Back,
        "forward" => Action::Forward,
        "scroll_page_down" => Action::Scroll(10),
        "scroll_page_up" => Action::Scroll(-10),
        "scroll_top" => Action::ScrollToTop,
        "scroll_bottom" => Action::ScrollToBottom,
        "scroll_down" => Action::Scroll(1),
        "scroll_up" => Action::Scroll(-1),
        "next_link" => Action::NextLink,
        "prev_link" => Action::PrevLink,
        "activate_link" => Action::ActivateLink,
        "toggle_help" => Action::ToggleHelp,
        "bookmark" => Action::Bookmark,
        _ => return None,
    })
}

fn strip_prefix_ci(prefix: &str, text: &mut &str) -> bool {
    if text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        *text = &text[prefix.len()..];
        true
    } else {
        false
    }
}

fn is_upper_char(code: KeyCode) -> bool {
    matches!(code, KeyCode::Char(c) if c.is_ascii_uppercase())
}

fn key_code(name: &str) -> Option<KeyCode> {
    if name.chars().count() == 1 {
        return name.chars().next().map(KeyCode::Char);
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" | "shift-tab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pgup" | "pageup" => KeyCode::PageUp,
        "pgdn" | "pagedown" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "f1" => KeyCode::F(1),
        "f2" => KeyCode::F(2),
        "f3" => KeyCode::F(3),
        "f4" => KeyCode::F(4),
        "f5" => KeyCode::F(5),
        "f6" => KeyCode::F(6),
        "f7" => KeyCode::F(7),
        "f8" => KeyCode::F(8),
        "f9" => KeyCode::F(9),
        "f10" => KeyCode::F(10),
        "f11" => KeyCode::F(11),
        "f12" => KeyCode::F(12),
        _ => return None,
    })
}

fn codes_equal(expected: KeyCode, actual: KeyCode) -> bool {
    match (expected, actual) {
        // Lowercase patterns are deliberately case-insensitive (terminals
        // disagree about the SHIFT bit), but an explicit uppercase pattern
        // must not fire on the lowercase key.
        (KeyCode::Char(a), KeyCode::Char(b)) if a.is_ascii_uppercase() => a == b,
        (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
        // Some terminals deliver Tab as a literal tab character.
        (KeyCode::Tab, KeyCode::Char('\t')) | (KeyCode::Char('\t'), KeyCode::Tab) => true,
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEvent;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn matches_plain_and_control_characters() {
        assert!(matches("q", &key(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(!matches("q", &key(KeyCode::Char('x'), KeyModifiers::NONE)));
        assert!(matches(
            "ctrl-c",
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
        assert!(matches(
            "ctrl-c",
            &key(KeyCode::Char('C'), KeyModifiers::CONTROL)
        ));
        assert!(!matches(
            "ctrl-c",
            &key(KeyCode::Char('c'), KeyModifiers::NONE)
        ));
        assert!(!matches(
            "ctrl-alt-c",
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn matches_shift_g_with_or_without_modifier_bit() {
        let with_bit = key(KeyCode::Char('G'), KeyModifiers::SHIFT);
        let uppercase_only = key(KeyCode::Char('G'), KeyModifiers::NONE);
        let lowercase = key(KeyCode::Char('g'), KeyModifiers::NONE);
        assert!(matches("shift-g", &with_bit));
        assert!(matches("shift-g", &uppercase_only));
        assert!(!matches("shift-g", &lowercase));
        assert!(matches("g", &lowercase));
    }

    #[test]
    fn matches_named_keys_and_tab_variants() {
        assert!(matches("enter", &key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(matches("esc", &key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(matches("pgdn", &key(KeyCode::PageDown, KeyModifiers::NONE)));
        assert!(matches("tab", &key(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(!matches("tab", &key(KeyCode::BackTab, KeyModifiers::NONE)));
        assert!(matches(
            "shift-tab",
            &key(KeyCode::BackTab, KeyModifiers::NONE)
        ));
        assert!(matches(
            "ctrl-shift-tab",
            &key(
                KeyCode::BackTab,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        ));
        assert!(!matches("ctrl-tab", &key(KeyCode::Tab, KeyModifiers::NONE)));
    }

    #[test]
    fn matches_alt_combinations() {
        assert!(matches("alt-left", &key(KeyCode::Left, KeyModifiers::ALT)));
        assert!(!matches(
            "alt-left",
            &key(KeyCode::Left, KeyModifiers::NONE)
        ));
    }

    #[test]
    fn ignores_non_key_events_and_releases() {
        assert!(!matches("q", &Event::Resize(80, 24)));
        let release = Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(!matches("q", &release));
    }

    #[test]
    fn resolve_uses_defaults_and_overrides() {
        let empty = BTreeMap::new();
        assert_eq!(
            resolve(&key(KeyCode::Char('q'), KeyModifiers::NONE), &empty),
            Some(Action::Quit)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('j'), KeyModifiers::NONE), &empty),
            Some(Action::Scroll(1))
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('G'), KeyModifiers::NONE), &empty),
            Some(Action::ScrollToBottom)
        );

        let mut map = BTreeMap::new();
        map.insert("quit".to_owned(), "x".to_owned());
        map.insert("scroll_down".to_owned(), "down|s".to_owned());
        assert_eq!(
            resolve(&key(KeyCode::Char('x'), KeyModifiers::NONE), &map),
            Some(Action::Quit)
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('q'), KeyModifiers::NONE), &map),
            None
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('s'), KeyModifiers::NONE), &map),
            Some(Action::Scroll(1))
        );
        assert_eq!(
            resolve(&key(KeyCode::Down, KeyModifiers::NONE), &map),
            Some(Action::Scroll(1))
        );
    }

    #[test]
    fn modifiers_are_accepted_in_any_order_and_extra_modifiers_rejected() {
        let all = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT;
        assert!(matches(
            "alt-ctrl-c",
            &key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )
        ));
        assert!(matches("shift-alt-ctrl-c", &key(KeyCode::Char('C'), all)));
        assert!(!matches(
            "ctrl-c",
            &key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )
        ));
        assert!(!matches(
            "alt-left",
            &key(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn explicit_uppercase_pattern_is_case_sensitive() {
        assert!(matches("G", &key(KeyCode::Char('G'), KeyModifiers::NONE)));
        assert!(!matches("G", &key(KeyCode::Char('g'), KeyModifiers::NONE)));
        // Lowercase patterns stay case-insensitive so SHIFT-bit differences
        // between terminals do not change bindings.
        assert!(matches("g", &key(KeyCode::Char('G'), KeyModifiers::NONE)));
        assert!(matches(
            "shift-g",
            &key(KeyCode::Char('G'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn resolve_supports_aliases_and_alternative_lists() {
        let mut map = BTreeMap::new();
        map.insert("scroll-down".to_owned(), " down | s ".to_owned());
        assert_eq!(
            resolve(&key(KeyCode::Down, KeyModifiers::NONE), &map),
            Some(Action::Scroll(1))
        );
        assert_eq!(
            resolve(&key(KeyCode::Char('s'), KeyModifiers::NONE), &map),
            Some(Action::Scroll(1))
        );
        // An override does not change any other binding.
        assert_eq!(
            resolve(&key(KeyCode::Char('k'), KeyModifiers::NONE), &map),
            Some(Action::Scroll(-1))
        );
    }
}
