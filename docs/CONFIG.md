# Configuration

`mil` stores user configuration as TOML. This document describes the **current**
schema implemented by the `mil-session` crate. It may still change before 1.0.

Every key is optional: omitted keys use the defaults below. Unknown keys are
silently ignored. Files are written atomically, so a crash cannot leave a
half-written config, session, or bookmarks file.

## File locations

| Variable | Effect |
| --- | --- |
| `MIL_CONFIG` | Use this file path directly. Relative paths are resolved against the current directory. |
| `XDG_CONFIG_HOME` | Base directory for config lookup (default `~/.config`). Must be absolute; relative values are ignored with a warning. |
| `XDG_STATE_HOME` | Base directory for mutable state (default `~/.local/state`). Must be absolute; relative values are ignored with a warning. |

Config lookup order (first match wins):

1. `MIL_CONFIG`, if set and non-empty;
2. `$XDG_CONFIG_HOME/mil/config.toml`;
3. `~/.config/mil/config.toml`;
4. built-in defaults, if no file exists.

Mutable state is not configured in `config.toml`. It always lives in
`$XDG_STATE_HOME/mil` (or `~/.local/state/mil`), in these files:

- `session.json` — tab/session snapshot
- `bookmarks.json` — bookmark collection

A missing state file, or an empty/whitespace-only one, is treated as an empty
collection.

## Complete annotated example

```toml
# ~/.config/mil/config.toml

homepage = "https://example.com"
search_engine = "https://www.google.com/search?q={}"
# `{}` is where the URL-encoded omnibox query is substituted. mil-session
# stores the template verbatim and performs no substitution itself.
# (Some docs call this placeholder `{query}`; the literal token here is `{}`.)

# user_agent = "mil/0.1 (+https://github.com/SaibaWaipu/mil)"  # omit for no override
# max_width = 120  # columns; omit to follow the terminal width

[theme]
palette = "auto"       # auto | dark | light | 16 | 256 | truecolor
# accent = "#89b4fa"   # optional colour override, stored verbatim
# link = "#89b4fa"     # optional colour override, stored verbatim

[keybindings]
"ctrl+t" = "new_tab"
"ctrl+w" = "close_tab"
"q" = "quit"

[images]
policy = "inline"      # inline | alt | sampling | omit
protocol = "auto"      # auto | iterm2 | sixel | kitty

[javascript]
enabled = false
timeout_ms = 1000

[network]
timeout_secs = 30
# proxy = "http://127.0.0.1:8080"  # omit for no proxy

[ui]
show_tab_bar = true
show_status_bar = true
smooth_scroll = false
```

Commented-out lines are optional keys shown for reference. Omitting a whole
table (`[theme]`, `[images]`, etc.) is equivalent to using all of its defaults.

## Defaults

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `homepage` | string | `"https://example.com"` | Page opened when no session is restored. |
| `search_engine` | string | `"https://www.google.com/search?q={}"` | Omnibox search template. `{}` is the query placeholder. |
| `user_agent` | string | omitted (`None`) | Optional `User-Agent` override; no default UA is injected by this crate. |
| `max_width` | integer (`u16`) | omitted (`None`) | Maximum layout width in columns. Omit to follow the terminal width; `0` means a literal zero width. |
| `theme.palette` | string | `"auto"` | Colour mode; see accepted values below. |
| `theme.accent` | string | omitted (`None`) | Optional accent colour override. |
| `theme.link` | string | omitted (`None`) | Optional link colour override. |
| `keybindings` | table of string → string | empty | Key sequence → action name map. No built-in bindings are merged by this crate. |
| `images.policy` | string | `"inline"` | How images are rendered. |
| `images.protocol` | string | `"auto"` | Inline image protocol. |
| `javascript.enabled` | boolean | `false` | JavaScript master switch. |
| `javascript.timeout_ms` | integer (`u64`) | `1000` | Per-script timeout in milliseconds. |
| `network.timeout_secs` | integer (`u64`) | `30` | Per-request timeout in seconds. |
| `network.proxy` | string | omitted (`None`) | Optional proxy URL. |
| `ui.show_tab_bar` | boolean | `true` | Show the tab bar. |
| `ui.show_status_bar` | boolean | `true` | Show the status bar. |
| `ui.smooth_scroll` | boolean | `false` | Animate scrolling. |

### Accepted string values

| Key | Accepted values |
| --- | --- |
| `theme.palette` | `auto`, `dark`, `light`, `16`, `256`, `truecolor` |
| `images.policy` | `inline`, `alt`, `sampling`, `omit` |
| `images.protocol` | `auto`, `iterm2`, `sixel`, `kitty` |

`user_agent`, `theme.accent`, `theme.link`, `network.proxy`, and all
`keybindings` values are free-form strings; they are stored verbatim and are
not validated by `mil-session`.

## Invalid values and parse errors

- An **unknown key** is ignored silently.
- An **invalid value for `theme.palette`, `images.policy`, or
  `images.protocol`** produces a `tracing::warn!` naming the key, the invalid
  value, the accepted values, and the default. The value is replaced by its
  default in memory. The file itself is not rewritten; a later `save()` will
  persist the corrected value.
- Any **wrong type or out-of-range number** (for example `homepage = 42`,
  `javascript.timeout_ms = "fast"`, or a negative `u64`) is a TOML parse
  error. `load()` returns an error with the config path; no fallback is used.
- A **missing config file**, an **empty file**, or a file containing only
  whitespace yields built-in defaults.

## State files

`session.json` is a JSON object; all fields are optional and default as shown:

```json
{
  "tabs": [
    {
      "url": "https://example.com",
      "title": "Example",
      "scroll_x": 0.0,
      "scroll_y": 12.5,
      "back": ["https://previous.example"],
      "forward": []
    }
  ],
  "active": 0
}
```

`active` is a zero-based index. An empty session (`"tabs": []`) loads and
saves normally. Scroll positions must be finite numbers; non-finite values are
written as `0.0`, and `null` in an older/hand-written snapshot is read as `0.0`.

`bookmarks.json` is likewise a JSON object:

```json
{
  "entries": [
    {
      "title": "Example",
      "url": "https://example.com",
      "tags": [],
      "created_at": 1730000000
    }
  ]
}
```

`created_at` is a Unix timestamp in seconds (`u64`). Malformed JSON is a hard
error, not an empty-collection fallback.

## Persistence details

Writes go to a hidden, uniquely named temporary file in the destination
directory; the file is flushed with `fsync`, renamed over the destination, and
the parent directory is fsynced on a best-effort basis. A failed write removes
the temporary file. On Unix, files are created with mode `0600` so proxy
settings, bookmarks, and browsing history are not world-readable — replacing
an existing file also resets its mode to `0600`.
