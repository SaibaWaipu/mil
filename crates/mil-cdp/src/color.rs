//! Conversion from CSS colour syntax to the 8-bit colours a terminal can show.
//!
//! Chrome's computed styles almost always use `rgb(...)` / `rgba(...)`, but the
//! parser also accepts hex colours, `hsl(...)`, and a small table of named
//! colours so hand-written test fixtures behave the same way as a real page.
//! Fully transparent colours resolve to `None` (no paint), which is what the
//! surface expects for "leave the existing background alone".

use mil_paint::Rgb;

/// Parse a CSS colour into terminal RGB, returning `None` for anything
/// transparent, unsupported, or malformed.
pub(crate) fn parse_color(input: Option<&str>) -> Option<Rgb> {
    let raw = input?.trim();
    if raw.is_empty() {
        return None;
    }
    let text = raw.to_ascii_lowercase();
    if text == "transparent" || text == "currentcolor" {
        return None;
    }

    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Some(inner) = text
        .strip_prefix("rgba(")
        .or_else(|| text.strip_prefix("rgb("))
    {
        return parse_rgb(inner.strip_suffix(')').unwrap_or(inner));
    }
    if let Some(inner) = text
        .strip_prefix("hsla(")
        .or_else(|| text.strip_prefix("hsl("))
    {
        return parse_hsl(inner.strip_suffix(')').unwrap_or(inner));
    }
    parse_named(&text)
}

fn parse_hex(hex: &str) -> Option<Rgb> {
    let mut digits = [0u8; 8];
    let mut len = 0;
    for ch in hex.chars() {
        let value = ch.to_digit(16)? as u8;
        if len == digits.len() {
            return None;
        }
        digits[len] = value;
        len += 1;
    }
    let channel = |hi: u8, lo: u8| hi.saturating_mul(16).saturating_add(lo);
    match len {
        3 => Some(Rgb::new(digits[0] * 17, digits[1] * 17, digits[2] * 17)),
        4 => {
            if digits[3] == 0 {
                None
            } else {
                Some(Rgb::new(digits[0] * 17, digits[1] * 17, digits[2] * 17))
            }
        }
        6 => Some(Rgb::new(
            channel(digits[0], digits[1]),
            channel(digits[2], digits[3]),
            channel(digits[4], digits[5]),
        )),
        8 => {
            if channel(digits[6], digits[7]) == 0 {
                None
            } else {
                Some(Rgb::new(
                    channel(digits[0], digits[1]),
                    channel(digits[2], digits[3]),
                    channel(digits[4], digits[5]),
                ))
            }
        }
        _ => None,
    }
}

/// A parsed CSS number together with whether it carried a `%` suffix.
fn css_number(token: &str) -> Option<(f64, bool)> {
    let token = token.trim();
    let (digits, percent) = match token.strip_suffix('%') {
        Some(rest) => (rest.trim(), true),
        None => (token, false),
    };
    let value = digits.parse::<f64>().ok()?;
    if !value.is_finite() {
        return None;
    }
    Some((value, percent))
}

fn channel(token: &str) -> Option<u8> {
    let (value, percent) = css_number(token)?;
    let scaled = if percent { value * 2.55 } else { value };
    Some(scaled.round().clamp(0.0, 255.0) as u8)
}

fn alpha(token: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
    let Some((value, percent)) = css_number(token) else {
        return false;
    };
    let value = if percent { value / 100.0 } else { value };
    value > 0.001
}

fn parse_rgb(inner: &str) -> Option<Rgb> {
    // Both the legacy `r, g, b, a` form and the modern `r g b / a` form occur
    // in computed styles, so normalise separators before splitting.
    let (color, alpha_part) = match inner.split_once('/') {
        Some((color, a)) => (color, Some(a.trim())),
        None => (inner, None),
    };
    let tokens: Vec<&str> = color
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.len() < 3 {
        return None;
    }
    let a = match alpha_part {
        Some(a) => Some(a),
        None if tokens.len() >= 4 => Some(tokens[3]),
        None => None,
    };
    if !alpha(a) {
        return None;
    }
    Some(Rgb::new(
        channel(tokens[0])?,
        channel(tokens[1])?,
        channel(tokens[2])?,
    ))
}

fn parse_hsl(inner: &str) -> Option<Rgb> {
    let (color, alpha_part) = match inner.split_once('/') {
        Some((color, a)) => (color, Some(a.trim())),
        None => (inner, None),
    };
    let tokens: Vec<&str> = color
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.len() < 3 || !alpha(alpha_part) {
        return None;
    }
    let (hue, _) = css_number(tokens[0])?;
    let (sat, sat_pct) = css_number(tokens[1])?;
    let (light, light_pct) = css_number(tokens[2])?;
    let s = if sat_pct { sat / 100.0 } else { sat };
    let l = if light_pct { light / 100.0 } else { light };
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s.clamp(0.0, 1.0);
    let hp = hue.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match hp.floor() as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |v: f64| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(Rgb::new(byte(r), byte(g), byte(b)))
}

fn parse_named(name: &str) -> Option<Rgb> {
    let rgb = match name {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "lime" => (0, 255, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "silver" => (192, 192, 192),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "purple" => (128, 0, 128),
        "teal" => (0, 128, 128),
        "navy" => (0, 0, 128),
        "orange" => (255, 165, 0),
        _ => return None,
    };
    Some(Rgb::new(rgb.0, rgb.1, rgb.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rgb_and_rgba() {
        assert_eq!(
            parse_color(Some("rgb(255, 0, 17)")),
            Some(Rgb::new(255, 0, 17))
        );
        assert_eq!(
            parse_color(Some(" rgba(1 2 3 / 0.5) ")),
            Some(Rgb::new(1, 2, 3))
        );
        assert_eq!(
            parse_color(Some("rgba(9, 8, 7, 1)")),
            Some(Rgb::new(9, 8, 7))
        );
    }

    #[test]
    fn transparent_colours_are_none() {
        assert_eq!(parse_color(Some("transparent")), None);
        assert_eq!(parse_color(Some("rgba(0, 0, 0, 0)")), None);
        assert_eq!(parse_color(Some("rgb(0 0 0 / 0%)")), None);
        assert_eq!(parse_color(Some("#00000000")), None);
        assert_eq!(parse_color(None), None);
        assert_eq!(parse_color(Some("")), None);
    }

    #[test]
    fn parses_hex_and_named_colours() {
        assert_eq!(parse_color(Some("#fff")), Some(Rgb::WHITE));
        assert_eq!(
            parse_color(Some("#aabbcc")),
            Some(Rgb::new(0xaa, 0xbb, 0xcc))
        );
        assert_eq!(parse_color(Some("#1234")), Some(Rgb::new(0x11, 0x22, 0x33)));
        assert_eq!(parse_color(Some("RED")), Some(Rgb::new(255, 0, 0)));
    }

    #[test]
    fn parses_percentages_and_hsl() {
        assert_eq!(
            parse_color(Some("rgb(100% 0% 0%)")),
            Some(Rgb::new(255, 0, 0))
        );
        assert_eq!(
            parse_color(Some("hsl(0, 100%, 50%)")),
            Some(Rgb::new(255, 0, 0))
        );
        assert_eq!(parse_color(Some("hsl(120 100% 50% / 0)")), None);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_color(Some("not-a-colour")), None);
        assert_eq!(parse_color(Some("rgb(1, 2)")), None);
        assert_eq!(parse_color(Some("rgb(a, b, c)")), None);
    }
}
