//! Translation from crossterm key events to CDP input commands.
//!
//! Printable characters are delivered with `Input.insertText` (which updates
//! focused form controls without synthesising physical key codes). Navigation
//! and editing keys use matched `keyDown`/`keyUp` pairs so pages that listen
//! for `keydown` behave normally.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

/// CDP modifier bit mask: Alt=1, Ctrl=2, Meta=4, Shift=8.
pub(crate) fn cdp_modifiers(modifiers: KeyModifiers) -> i64 {
    let mut bits = 0;
    if modifiers.contains(KeyModifiers::ALT) {
        bits |= 1;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        bits |= 2;
    }
    if modifiers.contains(KeyModifiers::SUPER)
        || modifiers.contains(KeyModifiers::HYPER)
        || modifiers.contains(KeyModifiers::META)
    {
        bits |= 4;
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        bits |= 8;
    }
    bits
}

/// A key name/code pair suitable for `Input.dispatchKeyEvent`.
#[derive(Debug, Clone)]
pub(crate) struct KeyDefinition {
    pub key: String,
    pub code: String,
    pub virtual_key: i64,
}

impl KeyDefinition {
    fn new(key: impl Into<String>, code: impl Into<String>, virtual_key: i64) -> Self {
        Self {
            key: key.into(),
            code: code.into(),
            virtual_key,
        }
    }
}

/// Map a crossterm key code to DOM key/code/virtual-key values.
pub(crate) fn key_definition(code: KeyCode) -> Option<KeyDefinition> {
    let definition = match code {
        KeyCode::Backspace => KeyDefinition::new("Backspace", "Backspace", 8),
        KeyCode::Enter => KeyDefinition::new("Enter", "Enter", 13),
        KeyCode::Left => KeyDefinition::new("ArrowLeft", "ArrowLeft", 37),
        KeyCode::Right => KeyDefinition::new("ArrowRight", "ArrowRight", 39),
        KeyCode::Up => KeyDefinition::new("ArrowUp", "ArrowUp", 38),
        KeyCode::Down => KeyDefinition::new("ArrowDown", "ArrowDown", 40),
        KeyCode::Home => KeyDefinition::new("Home", "Home", 36),
        KeyCode::End => KeyDefinition::new("End", "End", 35),
        KeyCode::PageUp => KeyDefinition::new("PageUp", "PageUp", 33),
        KeyCode::PageDown => KeyDefinition::new("PageDown", "PageDown", 34),
        KeyCode::Tab => KeyDefinition::new("Tab", "Tab", 9),
        KeyCode::BackTab => KeyDefinition::new("Tab", "Tab", 9),
        KeyCode::Delete => KeyDefinition::new("Delete", "Delete", 46),
        KeyCode::Insert => KeyDefinition::new("Insert", "Insert", 45),
        KeyCode::Esc => KeyDefinition::new("Escape", "Escape", 27),
        KeyCode::F(n) if n <= 24 => {
            KeyDefinition::new(format!("F{n}"), format!("F{n}"), i64::from(n) + 111)
        }
        KeyCode::Char(ch) => char_definition(ch)?,
        _ => return None,
    };
    Some(definition)
}

fn char_definition(ch: char) -> Option<KeyDefinition> {
    let upper = ch.to_ascii_uppercase();
    if upper.is_ascii_alphabetic() {
        return Some(KeyDefinition::new(
            ch.to_string(),
            format!("Key{upper}"),
            i64::from(u32::from(upper)),
        ));
    }
    if ch.is_ascii_digit() {
        return Some(KeyDefinition::new(
            ch.to_string(),
            format!("Digit{ch}"),
            i64::from(u32::from(ch)),
        ));
    }
    let (key, code, vk) = match ch {
        ' ' => (" ", "Space", 32),
        '.' => (".", "Period", 190),
        ',' => (",", "Comma", 188),
        '/' => ("/", "Slash", 191),
        '\\' => ("\\", "Backslash", 220),
        '\'' => ("'", "Quote", 222),
        ';' => (";", "Semicolon", 186),
        '[' => ("[", "BracketLeft", 219),
        ']' => ("]", "BracketRight", 221),
        '-' => ("-", "Minus", 189),
        '=' => ("=", "Equal", 187),
        '`' => ("`", "Backquote", 192),
        _ => return Some(KeyDefinition::new(ch.to_string(), "Unidentified", 0)),
    };
    Some(KeyDefinition::new(key, code, vk))
}

/// Whether a printable character can be sent with `Input.insertText`.
pub(crate) fn is_insertable(code: KeyCode, modifiers: KeyModifiers) -> bool {
    if !matches!(code, KeyCode::Char(_)) {
        return false;
    }
    // Shortcuts and dead-key combinations need real key events.
    !modifiers.intersects(
        KeyModifiers::CONTROL
            | KeyModifiers::ALT
            | KeyModifiers::SUPER
            | KeyModifiers::HYPER
            | KeyModifiers::META,
    )
}

/// Keys handled by the surrounding TUI rather than by the web page.
///
/// Quit (`Ctrl-C`, `Ctrl-Q`), the address bar (`Ctrl-L`), back (`Esc`), and all
/// non-key events are consumed here so the backend never double-handles them.
pub(crate) fn is_ui_event(event: &Event) -> bool {
    match event {
        Event::Key(key) => {
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                return true;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(
                    key.code,
                    KeyCode::Char('c')
                        | KeyCode::Char('C')
                        | KeyCode::Char('q')
                        | KeyCode::Char('Q')
                )
            {
                return true;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L'))
            {
                return true;
            }
            matches!(key.code, KeyCode::Esc)
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_bits_follow_cdp() {
        assert_eq!(cdp_modifiers(KeyModifiers::NONE), 0);
        assert_eq!(cdp_modifiers(KeyModifiers::ALT), 1);
        assert_eq!(cdp_modifiers(KeyModifiers::CONTROL), 2);
        assert_eq!(cdp_modifiers(KeyModifiers::SUPER), 4);
        assert_eq!(cdp_modifiers(KeyModifiers::SHIFT), 8);
        assert_eq!(
            cdp_modifiers(KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            10
        );
    }

    #[test]
    fn key_definitions_cover_common_keys() {
        let enter = key_definition(KeyCode::Enter).unwrap();
        assert_eq!(
            (enter.key.as_str(), enter.code.as_str(), enter.virtual_key),
            ("Enter", "Enter", 13)
        );
        let left = key_definition(KeyCode::Left).unwrap();
        assert_eq!(left.key, "ArrowLeft");
        let a = key_definition(KeyCode::Char('A')).unwrap();
        assert_eq!(
            (a.key.as_str(), a.code.as_str(), a.virtual_key),
            ("A", "KeyA", 65)
        );
        assert_eq!(key_definition(KeyCode::F(5)).unwrap().virtual_key, 116);
        assert!(key_definition(KeyCode::Null).is_none());
    }

    #[test]
    fn printable_characters_are_insertable() {
        assert!(is_insertable(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(!is_insertable(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!is_insertable(KeyCode::Enter, KeyModifiers::NONE));
    }
}
