# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

> The core browser is implemented and verified in this tree: 133 workspace
> unit/integration tests (mil 13, mil-cdp 28, mil-dom 10, mil-js 14, mil-net 17,
> mil-paint 8, mil-session 15, mil-tui 28), one ignored real-browser integration
> test in `mil-cdp`, and 18 PTY end-to-end scenarios. `cargo clippy --workspace
> --all-targets -- -D warnings` and `cargo fmt --all -- --check` are clean, and
> local HTTP smoke tests confirmed page rendering and JavaScript mutations.
> There is no published crates.io release yet. See the [README](README.md) and
> [roadmap](docs/ROADMAP.md) for the distinction between implemented and
> planned behaviour.

### Added

- Seven-crate workspace (`mil-app`, `mil-net`, `mil-dom`, `mil-paint`, `mil-tui`,
  `mil-js`, `mil-session`) producing the `mil` binary, with a stable-Rust build
  and CI for fmt, clippy, tests and release builds on Linux, macOS and Windows,
  plus the optional `mil-cdp` backend as an eighth crate.
- Optional full-JavaScript CDP backend (`mil-cdp`): launches an external
  `chrome-headless-shell`/Chromium via `chromiumoxide` 0.9.1, lets Chrome run
  real scripts and CSS layout, extracts DOM text/image rectangles with a JS
  `TreeWalker` (`getBoundingClientRect()` + `getComputedStyle()`), paints them
  into the same `mil_paint::Surface` and reuses the existing TUI. `MIL_CHROME`
  selects the executable; CDP costs ~100–300 MiB per instance and does not
  bypass bot challenges.
- Real HTML/CSS parsing, cascade and layout through exactly pinned Blitz crates:
  `blitz-dom =0.2.4`, `blitz-html =0.2.0`, `blitz-paint =0.2.1`,
  `blitz-traits =0.2.0`.
- Terminal rendering pipeline: `Page::text_runs()` recovers source text and
  styles from resolved inline layouts, `blitz-paint` supplies backgrounds and
  image rectangles, `render_page()` builds a `mil-paint::Surface`, and `mil-tui`
  writes it into a ratatui `Buffer` with inline images via ratatui-image 11
  (iTerm2 OSC1337, Kitty, Sixel, half-blocks fallback or alt text).
- `clap` CLI: optional positional `URL`, `--config PATH`, `--no-session`,
  `--no-js`, `--engine <blitz|cdp>` (default `blitz`), `--chrome <PATH>`,
  repeatable `-v`, plus `--help` and `--version`.
- A non-blocking `cdp-e2e` CI job that installs a headless shell with
  `@puppeteer/browsers`, locates it, builds `mil` and runs a smoke with
  `MIL_CHROME` set.
- HTTP layer with `reqwest` + `rustls`, a manual in-memory cookie jar, up to ten
  redirects, gzip/brotli decoding, request timeouts and a bounded in-memory
  response cache; `Client::post_form` for `application/x-www-form-urlencoded`.
- Tabs with independent history and scroll state, session snapshots in the XDG
  state directory, bookmarks, back/forward navigation and a tab bar.
- Form discovery and serialization in `mil-dom` (`forms`, `serialize_form`,
  `serialize_form_encoded`, `set_field_value`, `toggle_field`) with tests for
  GET/POST pairs, submitters, checkboxes, radios and selects.
- Opt-in partial JavaScript via `rquickjs` 0.14: `JsDomMirror`, recorded
  `DomMutation` report, `apply_report`, bounded virtual timers, inline script
  execution and `Page::refresh_title()` after mutations. Enabled through
  `[javascript] enabled`; forced off with `--no-js` (Blitz mode only).
- TOML configuration with built-in defaults, `MIL_CONFIG`/XDG path handling,
  keybinding overrides, and atomic `0600` writes for config, session and
  bookmarks. Search queries use the configured template's `{}` placeholder.
- Keyboard shortcuts and mouse wheel/click navigation documented in the README;
  configuration schema documented in `docs/CONFIG.md`.

### Changed

- Documentation rewritten to reflect the implemented browser: feature markers
  now distinguish ✅ implemented from 🚧 partial or planned work in
  `README.md`, `docs/ROADMAP.md`, `docs/ARCHITECTURE.md` and this changelog.
- Blitz and companion crates are pinned to exact `=x.y.z` versions instead of
  caret ranges; upgrades are deliberate, tested changes.
- JavaScript is opt-in at runtime (config/flag) rather than a compile-time
  Cargo feature; it is disabled by default in Blitz mode. `--no-js` and
  `[javascript] enabled` are Blitz-only: CDP mode always runs full JavaScript.
  CDP reads the DOM as a snapshot re-read via `snapshot()` after interactions,
  shares one browser engine across tabs, and loosely filters UI shortcuts that
  can reach the page.
- The CLI takes a positional URL; the previously planned `--url` and
  `--new-session` flags became a positional argument and `--no-session`.
- Text rendering no longer relies on reverse-mapping Blitz glyph IDs: source
  graphemes come from DOM inline layouts, with CSS advances approximated to cell
  positions. Bold, italic and underline are applied as cell attributes.
- Session, bookmark and config files are written atomically via a private
  temporary file, fsync and rename; on Unix their mode is `0600`.
- Roadmap phases P0–P2 and P5 are recorded as substantially implemented, P3/P4
  as partial by design, and P6 as in progress; acceptance criteria were kept and
  annotated with the remaining gaps.

### Fixed

- Non-finite scroll offsets are written as `0.0` instead of JSON `null`, and
  older snapshots containing `null` load as `0.0` rather than failing.
- Failed writes no longer leave temporary files behind, and replacing an
  existing world-readable config/session/bookmarks file resets its mode to
  `0600` instead of preserving loose permissions.
- Relative `XDG_CONFIG_HOME`/`XDG_STATE_HOME` overrides are ignored with a
  warning, while an explicit relative `MIL_CONFIG` path is still accepted.
- Script errors are logged without aborting the page load, unapplicable JS
  mutations are skipped with a warning, and title changes made by scripts are
  re-read before repaint via `Page::refresh_title()`.
- Link activation resolves relative `href` values against the page URL and hit
  tests link rectangles, including after scrolling; off-screen images are
  clipped rather than emitted into the viewport.
- The cookie jar rejects explicit single-label public-suffix `Domain`
  attributes (for example `Domain=com`) to prevent supercookies.

[Unreleased]: https://github.com/SaibaWaipu/mil/compare/main...HEAD
