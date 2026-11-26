# Roadmap

This document records the implementation phases for `mil` and their status in the
current tree. Markers: **[x]** implemented and covered by tests or a local smoke
test, **[~]** partially implemented (the stated limitation is intentional or
still open), **[ ]** not implemented. A phase is "substantially implemented"
when its core path works end to end, even if individual criteria remain open.

Verification for this revision: 133 workspace unit/integration tests (mil 13,
mil-cdp 28, mil-dom 10, mil-js 14, mil-net 17, mil-paint 8, mil-session 15,
mil-tui 28), one ignored real-browser integration test in `mil-cdp`, and 18 PTY
end-to-end scenarios. `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo fmt --all -- --check` are clean, and local HTTP smoke tests confirmed
page rendering and JavaScript mutations. The optional CDP backend was exercised
with real JavaScript on public sites; there is no published crates.io release
yet.

## P0 — Scaffold and spike — `substantially implemented`

Establish the workspace, prove the core rendering idea, and get a usable
terminal lifecycle.

Acceptance criteria:

- [x] Cargo workspace with the seven default-browser crates (`mil-app`,
      `mil-net`, `mil-dom`, `mil-paint`, `mil-tui`, `mil-js`, `mil-session`) plus
      the optional `mil-cdp` backend — eight crates total — and a `mil` binary
      builds on stable Rust.
- [~] **Rendering spike:** a static HTML/CSS fixture is parsed by `blitz-html`,
      resolved to a `BaseDocument`, lowered through `mil-paint` into a cell
      grid, and printed with correct text, colours and basic block layout.
      Unit tests assert cell contents and paint records; dedicated committed
      snapshot fixtures are still a P6 item.
- [x] `clap` CLI parses a positional `URL` (rather than the originally planned
      `--url`), `--config`, `--no-session`, `--no-js`, repeatable `-v`, and
      Clap's `--help`/`--version`. `--new-session` was replaced by the inverse
      `--no-session`.
- [~] Terminal lifecycle: raw mode and the alternate screen are entered and
      restored through ratatui; `q`, `Ctrl-C` and `Ctrl-Q` exit cleanly and
      mouse capture is disabled on exit. Resize events redraw the chrome and
      viewport, but the document is not re-laid-out for the new dimensions
      (layout uses the size captured at navigation time).
- [x] Config loading from `~/.config/mil/config.toml` (with `MIL_CONFIG` and XDG
      overrides) with safe defaults when the file is absent.
- [x] CI workflow covers fmt, clippy and tests on Linux, plus release builds on
      Linux, macOS and Windows; local fmt/clippy/test runs are clean.

## P1 — Browsing core — `substantially implemented`

Make single-page navigation useful.

Acceptance criteria:

- [x] `mil-net` fetches `http://` and `https://` URLs via `reqwest` + `rustls`,
      with an in-memory cookie jar, up to ten redirects, gzip/brotli decoding
      and a request timeout; a bounded in-memory HTTP cache fronts GETs.
- [x] Responses are parsed into a `BaseDocument` and rendered end to end.
      `mil-dom/tests/page.rs` covers static fixtures and a local HTTP smoke test
      confirmed rendering in the binary.
- [~] Inline `<style>` rules affect layout through Blitz's parser and cascade.
      Linked external stylesheets are not loaded by `mil`: no custom
      network-backed subresource provider is installed, so that path remains
      🚧.
- [x] Keyboard line/page scrolling and top/bottom navigation are implemented
      and tested at the keymap level. 🚧 Zoom/DPI-like scaling is not wired up.
- [~] Link activation: mouse click uses rectangles derived from text runs
      inside `<a href>` and resolves relatives against the page URL; `Enter`
      follows the first link. Hovering does not yet show the target URL, and
      `Tab`/`Shift-Tab` currently scroll instead of moving a visible focus ring.
- [x] Back/forward history within a tab, with URL and scroll state restored via
      the tab model.
- [~] Integration tests cover fetch → parse → render (`mil-dom` fixture test)
      and redirect/cookie/404 behaviour (`mil-net` local server tests); the
      full click-through path is verified manually against a local server.

## P2 — Tabs and session restore — `substantially implemented`

Support multiple independent documents.

Acceptance criteria:

- [x] Each tab owns independent URL, title, document, surface, scroll position
      and back/forward stacks; switching tabs preserves all of it. 🚧 Per-tab
      zoom is not implemented.
- [x] Tab bar widget shows titles, marks the active tab and shows a loading
      indicator; new tab, close tab, next tab and previous tab keybindings
      work.
- [~] Session snapshots are JSON in `~/.local/state/mil/session.json` with
      tabs, URLs, titles, history and scroll offsets. Timestamps and zoom are
      not recorded.
- [x] On startup `mil` restores the previous session; `--no-session` bypasses
      restore and saving. Malformed or empty snapshots are logged and fall back
      to the homepage without panicking.
- [x] `mil-session` tests cover a snapshot round-trip with two tabs at distinct
      scroll positions, plus non-finite scroll sanitization.

## P3 — JavaScript via rquickjs — `implemented, partial by design (Blitz); full JS via optional CDP`

Add opt-in scripting with a deliberately small DOM/BOM surface, and provide an
escape hatch for pages that need real JavaScript.

Acceptance criteria:

- [~] JavaScript is opt-in **at runtime** (`[javascript] enabled`, default
      `false`) and can be forced off with `--no-js`. There is no compile-time
      `javascript` Cargo feature: `mil-js` is always compiled.
- [~] A mirrored DOM facade exposes `document`, `getElementById`,
      `querySelector`/`querySelectorAll`, `textContent`, limited `innerHTML`,
      get/set attribute, class and style properties, listeners and `click`.
      Selector support is limited to `#id`, `.class`, bare tags, `a[href]` and
      `*`; `appendChild`/`removeChild` and DOM creation are explicit non-goals.
- [x] Inline `<script>` blocks without `src` are collected from the parsed
      document and executed after layout; mutations are applied to the live
      document, `Page::refresh_title()` re-reads the title and the page is
      re-resolved before repaint. Script errors are logged and never abort a
      page load.
- [~] `setTimeout`/`setInterval` run on a bounded virtual clock (at most 16
      callbacks / 1000 ms) inside one run; a fresh context and mirror per page
      means no timer survives navigation. They are not driven by the Tokio
      loop. `fetch` records a navigation and returns a placeholder with a
      no-op `then`/`catch`; there is no network continuation.
- [~] `javascript.enabled` is honoured; `javascript.timeout_ms` caps the
      virtual timer budget (itself capped at 1000 ms) and script errors are
      reported via `tracing`. A single long-running synchronous script is not
      preempted by a wall-clock kill switch.
- [x] Non-goals are documented in `mil-js`: no Node APIs, external modules,
      arbitrary file access, promises, XHR, `localStorage`,
      `requestAnimationFrame`, complex selectors, workers, WASM or event
      dispatch beyond `click()`. SPAs are expected to fail **in Blitz mode**.
- [x] **The full-JavaScript gap is addressed by the optional CDP backend**
      (`mil-cdp`, chromiumoxide 0.9.1). Chrome does fetching, scripting and
      layout; a JS walker measures text/images with `getBoundingClientRect()`
      and `getComputedStyle()` and `mil-cdp` paints them into the same
      `mil_paint::Surface` and TUI. `--chrome PATH` and `MIL_CHROME` select the
      executable; `--no-js` and `[javascript] enabled` are Blitz-only.
- [~] CDP trade-offs are intentional, not planned away: ~100–300 MiB per browser
      instance, an external binary, DOM snapshots re-read via `snapshot()`
      instead of a live mirror, one engine serving all tabs, and UI shortcuts
      that can leak into the page. Bing/Wikipedia/HN/GitHub render with full JS;
      Google still serves a `/sorry/index` bot challenge from datacenter IPs.

## P4 — Forms and login — `library-level support, partial by design`

Support realistic form-based interaction.

Acceptance criteria:

- [~] Form discovery and control state (text inputs, textareas, selects,
      checkboxes and radios) are implemented in `mil-dom`, including Blitz's
      live editor state. Focus traversal with a visible focus ring is 🚧.
- [~] `Page::set_field_value` and `Page::toggle_field` mutate controls;
      `Page::serialize_form` / `serialize_form_encoded` build correct
      `application/x-www-form-urlencoded` name/value pairs, and `mil-net::post_form`
      sends them. Interactive in-page text editing and a UI submit action are
      🚧, so login flows are not yet usable end to end.
- [~] `Set-Cookie` is parsed into a shared in-memory jar and resent on matching
      requests; `mil-net` can save/load persistent cookies as JSON, but the
      binary does not wire this up. Session/expiry attributes are handled by
      `cookie_store`; there is no public-suffix list.
- [ ] An automated integration test that logs in to a local server through the
      browser UI is still open. Form serialization itself is covered by
      `mil-dom/tests/forms.rs`.

## P5 — Images, mouse, and themes — `substantially implemented`

Improve fidelity and ergonomics.

Acceptance criteria:

- [x] Capability detection via ratatui-image's `Picker` selects iTerm2
      (OSC1337), Sixel or Kitty graphics, falling back to half blocks or alt
      text. 🚧 The `images.protocol` config value is parsed and validated but
      is not yet passed to the picker as a manual override.
- [~] `images.policy` (`inline`, `alt`, `sampling`, `omit`) is parsed and
      validated in `mil-session`; the binary currently auto-detects and uses
      alt text when no graphics protocol is available. Configurable per-policy
      behaviour is 🚧.
- [x] Inline images are placed at layout-derived rectangles, scaled to the
      viewport and clipped outside it; protocol instances are cached per image.
- [~] Mouse wheel scrolls and left click activates links. Pointer hover status
      and drag text selection are 🚧.
- [~] Keybinding overrides from TOML are honoured by `mil-tui`. Theme and UI
      palette fields are parsed and validated but are not yet applied to the
      rendered chrome.
- [ ] Visual snapshot tests for each protocol/policy combination are 🚧.

## P6 — Hardening and release — `in progress`

Make the project maintainable and distributable.

Acceptance criteria:

- [~] `mil-paint` unit tests cover cell geometry, wide glyphs, text styles and
      recorded fills/images; committed visual snapshot fixtures for text,
      wrapping, tables, images and focus/selection states are 🚧.
- [~] Local server tests in `mil-net` exercise redirects, cookies, 404s and
      timeouts without panics. End-to-end browser tests for malformed HTML are
      still manual.
- [ ] Criterion benches for parse, resolve, paint and frame time are 🚧; no
      results are recorded yet.
- [x] User docs exist: README, `docs/CONFIG.md`, `docs/ARCHITECTURE.md`, a
      keybindings reference in the README, and limitations/troubleshooting
      notes. They are updated as behaviour changes.
- [~] A release workflow builds five target triples with SHA-256 checksums and
      generated release notes. No tagged release has been cut and no per-OS
      manual smoke-test records have been published yet.

## Limitations and non-goals

- Not a full browser engine: no WebGL, service workers, WebSockets, web
  notifications, geolocation, media playback, extensions, PDFs or DRM.
- In Blitz mode, JavaScript is a deliberately minimal, opt-in facade — not
  Node.js and not a complete ECMAScript/DOM implementation. Modern SPA
  frameworks are expected to fail there; the optional CDP backend runs the
  page's real JavaScript instead.
- **No bundled browser.** The CDP backend is optional and never ships a
  Chromium binary: users must install `chrome-headless-shell` (or point at a
  compatible Chromium) themselves. Blitz stays the lightweight, self-contained
  default.
- **No bot-challenge or CAPTCHA bypass.** Google Search from a datacenter IP
  returns `/sorry/index` even with full JavaScript in CDP mode. That is
  anti-bot detection, not a renderer gap, and defeating it is out of scope.
- No sandbox isolation beyond process-level privileges; do not browse
  untrusted sites with sensitive credentials. The Blitz JS memory limit and
  timer budget are robustness measures, not a security boundary; CDP launches
  Chrome with `--no-sandbox` and does not add a security boundary either.
- Terminal rendering is inherently limited: absolute positioning, precise
  fonts, sub-cell precision and pixel-perfect CSS are out of scope. Text
  fidelity depends on terminal capabilities.
- Layout fidelity with Blitz is bounded by Blitz's own CSS support; unsupported
  features degrade gracefully rather than being reimplemented here. Strokes,
  box-shadow, gradients, SVG and custom paints are ignored; clip layers are
  approximated and there is no blend compositing.
- `BaseDocument` is `!Send`, so the browser remains on a current-thread Tokio
  runtime; moving documents between threads is not planned.
- Mobile/touch input and accessibility integrations (screen readers) are not
  planned for 1.0.
