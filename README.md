# mil

> A fast, lightweight terminal web browser — real CSS layout, rendered to your terminal cell grid.

`mil` (pronounced "mill") is a keyboard-first web browser that runs entirely in your terminal. It uses the Blitz HTML/CSS engine to parse and lay out pages, then lowers the result into a terminal cell grid with `mil-paint` and ratatui. It is designed to feel like a native Unix command: `mil [URL]`, with sane defaults and no graphical desktop required.

**Status:** the core browser is implemented and verified in this repository. The workspace has 133 unit/integration tests (mil 13, mil-cdp 28, mil-dom 10, mil-js 14, mil-net 17, mil-paint 8, mil-session 15, mil-tui 28), one ignored real-browser integration test in `mil-cdp`, and 18 end-to-end tests that drive the real binary through a PTY against a local HTTP server. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` are clean. An optional CDP backend provides full JavaScript; it needs an external `chrome-headless-shell` binary. There is no published crates.io release yet. Below, ✅ means implemented in this tree; 🚧 means partial, not yet usable end-to-end, or planned. See [Status and roadmap](#status-and-roadmap).

## Why it exists

- **Real layout instead of text dumps.** Blitz (`blitz-dom` 0.2.4, `blitz-html` 0.2.0, `blitz-paint` 0.2.1, `blitz-traits` 0.2.0, pinned exactly) provides genuine HTML parsing, CSS cascade and layout.
- **Hybrid text rendering.** Blitz paints font glyph IDs that cannot be mapped back to source text, so `mil` walks the resolved DOM inline layouts for text and uses Blitz's paint output for backgrounds and images. Cells stay selectable and readable.
- **Small footprint by default.** No Chromium and no GUI toolkit in the default path — just a terminal and a Rust binary. It is much smaller than Chromium, though not tiny: Stylo, Taffy and Parley are linked in. The optional CDP backend trades that footprint for full JavaScript.
- **Optional full JavaScript.** `--engine cdp` drives an external headless Chrome through the Chrome DevTools Protocol and lowers its page into the same terminal surface. Blitz stays the lightweight default.
- **Native command ergonomics.** `mil https://example.com`, sensible defaults, TOML configuration in an XDG location, and mutable state kept out of the config directory.
- **Keyboard-first.** Vim-like scrolling, tabs, history, bookmarks, link activation and mouse wheel/click support, all without leaving the terminal.

## Features

### Core

- ✅ `mil [URL]` command-line interface with optional URL, `--config`, `--no-session`, `--no-js`, `--engine` and `--chrome`, repeatable `-v`, plus Clap's `--help` and `--version`.
- ✅ Real HTML/CSS parsing and layout via exactly pinned Blitz 0.2.x crates (default engine).
- ✅ An opt-in CDP engine for full JavaScript (🚧 requires an external browser binary and ~100–300 MiB; see [Full JavaScript mode](#full-javascript-mode)).
- ✅ Terminal cell-grid painting with `mil-paint`, ratatui and crossterm; bold, italic and underline become cell attributes.
- ✅ Keyboard scrolling (line, page, top, bottom), reload, history back/forward, tabs, bookmarks (`b`) and mouse wheel/click navigation.
- ✅ Resolved text placed into cells from DOM inline layouts, with backgrounds and inline images composited on top.

### Browsing

- ✅ Multiple independent tabs with a tab bar and keyboard switching.
- ✅ Session restore from `~/.local/state/mil/session.json`; `--no-session` bypasses both restore and save. Corrupt snapshots fall back to the homepage.
- ✅ Bookmarks stored in `~/.local/state/mil/bookmarks.json`.
- ✅ HTTP(S) fetching with rustls, bounded redirects, gzip/brotli decoding, request timeouts and a bounded in-memory HTTP cache.
- ✅ In-memory cookie jar: `Set-Cookie` is parsed and cookies are resent on matching requests. 🚧 Persisting the jar across runs is not wired into the binary yet (`mil-net` exposes save/load helpers).
- ✅ Form discovery and `application/x-www-form-urlencoded` GET/POST serialization (`Page::serialize_form_encoded` + `Client::post_form`), with tests. 🚧 Focus traversal, in-page editing and an end-to-end submit action are not wired into the TUI yet.

### Scripting and media

- ✅ Partial JavaScript via `rquickjs` 0.14, **opt-in and disabled by default**: `document.title`/`cookie`/`write`, `getElementById`, `querySelector`/`querySelectorAll`, `textContent`, limited `innerHTML`, `style`/`classList`/attributes, `addEventListener`/`click`, `setTimeout`/`setInterval`, `console`/`alert`, `location` and a placeholder `fetch`. Inline scripts run against a mirrored DOM; their writes are applied to the live document and relayout is triggered.
- ✅ Inline images through ratatui-image 11: capability detection selects iTerm2 (OSC1337), Kitty or Sixel, with a half-blocks fallback (or alt text when no graphics protocol is available).
- ✅ Full JavaScript via the optional CDP backend (chromiumoxide 0.9.1 + `chrome-headless-shell`): Chrome parses, scripts and lays out the page; a JS walker measures text and image rectangles which are painted into the same `mil-paint::Surface` and shown in the same TUI. 🚧 JS is always on in this mode, so `--no-js` has no effect; interactions use DOM snapshots and some UI shortcuts can reach the page.
- 🚧 Full JavaScript compatibility in the default Blitz engine, and per-policy/per-protocol image overrides in the running binary. Full JS is available through `--engine cdp` (see below).

### Configuration and platform

- ✅ TOML configuration at `~/.config/mil/config.toml` (with `MIL_CONFIG` and XDG overrides), XDG state paths, and atomic `0600` writes for config, session and bookmarks.
- ✅ Linux and macOS are the primary targets; Windows builds are covered by CI but terminal/path behavior is best-effort.

## Installation

The crate is not published on crates.io yet, so install from source:

```sh
git clone https://github.com/SaibaWaipu/mil.git
cd mil
cargo install --path crates/mil-app
```

This installs the `mil` binary. A recent stable Rust toolchain is required; see [CONTRIBUTING.md](CONTRIBUTING.md) for development setup. To run without installing:


```sh
cargo run -p mil -- https://example.com
```

CDP mode additionally needs a Chrome-compatible executable. If you do not already have Chromium installed, fetch `chrome-headless-shell` with:

```sh
npx @puppeteer/browsers install chrome-headless-shell@stable --path ~/.cache/puppeteer
```

Then point `mil` at it with `--chrome PATH` or `MIL_CHROME=PATH`. Without a browser binary, the default Blitz mode still works; `--engine cdp` fails with a visible error rather than silently falling back.

## Quick start

```sh
mil https://example.com   # open an absolute URL
mil                       # restore the previous session, or open the homepage
mil example.org           # bare hosts are treated as https:// URLs
mil "rust terminal browser"  # anything else is a search query
```

A URL without a scheme is treated as `https://` when it looks like a host. Otherwise the input is URL-encoded and substituted into `search_engine` at its `{}` placeholder. With no argument, `mil` restores the saved session (unless `--no-session` is given) or opens the configured homepage.

## Command line

| Flag | Effect |
| --- | --- |
| `URL` | Optional URL, bare host, or search query to open on startup. |
| `--config PATH` | Load an alternate TOML configuration file. |
| `--no-session` | Do not restore a previous session, and do not save one on exit. |
| `--no-js` | Disable JavaScript even when enabled in the config. Blitz only; no effect with `--engine cdp`. |
| `--engine <blitz\|cdp>` | Select the rendering backend. Defaults to `blitz`; `cdp` runs full JavaScript in an external headless Chrome. |
| `--chrome <PATH>` | Chrome/Chromium-compatible executable for CDP mode; takes precedence over `MIL_CHROME` and auto-detection. |
| `-v`, `--verbose` | Increase logging verbosity; repeatable (`warn` → `info` → `debug` → `trace`). |
| `-h`, `--help` | Show Clap's help output. |
| `-V`, `--version` | Show the version. |

## Full JavaScript mode

The default Blitz engine runs a small, opt-in JavaScript facade. For pages that need real JavaScript, opt into the CDP backend:

```sh
mil --engine cdp https://github.com
```

This launches `chrome-headless-shell` (or another Chromium-compatible binary) via chromiumoxide 0.9.1, lets Chrome run the page's own JavaScript and CSS, walks the rendered DOM with a JS `TreeWalker` (`getBoundingClientRect()` plus `getComputedStyle()`), and converts the measured text and image rectangles into the same `mil_paint::Surface` the Blitz path paints. The TUI, keybindings, tabs, history and session code are unchanged.

**Costs and caveats — read these before opting in:**

- A browser instance costs roughly **100–300 MiB** of resident memory plus a child process. You supply the executable; `mil` does not bundle or download one. Blitz remains the lightweight default.
- `--no-js` has **no effect** in CDP mode: JavaScript is always on. `[javascript] enabled` in `config.toml` applies only to Blitz.
- The terminal sees a **snapshot** of the DOM taken on navigation/load. Interactions re-read it with `snapshot()` without reloading, so in-page state survives, but live DOM mutations are not pushed to the screen until the next snapshot.
- **One CDP engine serves all tabs.** Because the app cannot tell which keys the TUI consumed, UI shortcuts (`q`, `Ctrl-L`, `Ctrl-T`, `r`, find-prompt typing, …) can also reach the page.
- **Bot challenges are not bypassed.** With a datacenter IP, Google Search still returns a `/sorry/index` challenge even in CDP mode. That is anti-bot detection, not a renderer limitation. Bing, Wikipedia, Hacker News and GitHub render with full JavaScript.

Relevant environment variables:

| Variable | Effect |
| --- | --- |
| `MIL_CHROME` | Path to the Chrome-compatible executable. `--chrome PATH` overrides it; otherwise `mil` checks well-known locations and then `PATH`. |
| `MIL_IMAGE_PROTOCOL` | `none`, `off` or `halfblocks` skips terminal graphics probing (useful in tmux/CI). Applies to both engines. |
| `MIL_LOAD_STYLESHEETS` | Blitz only: fetch linked CSS. Irrelevant in CDP mode, where Chrome loads stylesheets itself. |

## Keyboard and mouse

| Key | Action |
| --- | --- |
| <kbd>q</kbd>, <kbd>Ctrl-C</kbd>, <kbd>Ctrl-Q</kbd> | Quit |
| <kbd>Ctrl-L</kbd> | Focus the URL bar; <kbd>Esc</kbd> leaves it |
| <kbd>Ctrl-T</kbd> / <kbd>Ctrl-W</kbd> | New tab / close current tab |
| <kbd>Ctrl-Tab</kbd> / <kbd>Ctrl-Shift-Tab</kbd> | Next / previous tab |
| <kbd>j</kbd> / <kbd>k</kbd> (or <kbd>↓</kbd>/<kbd>↑</kbd>) | Scroll one row down / up |
| <kbd>Ctrl-D</kbd> / <kbd>Ctrl-U</kbd> (or <kbd>PgDn</kbd>/<kbd>PgUp</kbd>) | Scroll one page down / up |
| <kbd>g</kbd> / <kbd>G</kbd> | Go to top / bottom |
| <kbd>Enter</kbd> | Follow a link or submit the URL bar |
| <kbd>Tab</kbd> / <kbd>Shift-Tab</kbd> | Link navigation (currently a short scroll; focus ring 🚧) |
| <kbd>/</kbd> | Find in page (query collection works; highlighting/matching 🚧) |
| <kbd>r</kbd> | Reload |
| <kbd>Alt-Left</kbd> / <kbd>Backspace</kbd>, <kbd>Alt-Right</kbd> | Back / forward in history |
| <kbd>b</kbd> | Bookmark the current tab |
| <kbd>?</kbd> | Help hint in the status bar (full help screen 🚧) |
| Mouse wheel / left click | Scroll / activate a link under the cursor |

Bindings are resolved from `DEFAULT_BINDINGS` in `mil-tui` and can be overridden per action in `[keybindings]`; see [docs/CONFIG.md](docs/CONFIG.md).

## How it works

1. `mil-net` fetches the document (cache lookup, cookies, redirects) and `mil-dom::Page::from_html` parses it with `blitz-html` into a `BaseDocument`.
2. `doc.resolve()` runs the CSS cascade and layout. Because Blitz emits opaque font glyph IDs rather than source text, `Page::text_runs()` recovers graphemes and styles from the resolved inline layouts, while `blitz-paint` supplies element backgrounds and image rectangles.
3. `render_page()` writes those pieces into a `Surface`: a cell grid (glyph, foreground/background colour, text attributes) plus placed PNG images.
4. `mil-tui` copies the visible cells into a ratatui `Buffer` and hands images to ratatui-image, which emits the appropriate terminal graphics escapes. Ratatui diffs the buffer against the previous frame when drawing.
5. If JavaScript is enabled in Blitz mode, inline scripts run in a fresh rquickjs context against a `JsDomMirror`. Mutations are applied to the live document, `Page::refresh_title()` re-reads the title, and the page is resolved and repainted again.
6. With `--engine cdp`, steps 1–2 and 5 are replaced: Chrome fetches, scripts and lays out the page; `mil-cdp` evaluates a JS walker, paints the returned spans into a `Surface`, and both paths rejoin at `mil-tui` (step 4). The Blitz JS facade is not invoked in CDP mode.

The event loop runs on a current-thread Tokio runtime because `blitz_dom::BaseDocument` is `!Send`; see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for details, including the CDP backend's process lifecycle and input forwarding.

## Configuration

Configuration is read from `~/.config/mil/config.toml` (or `$XDG_CONFIG_HOME/mil/config.toml`, or the file named by `MIL_CONFIG`). A missing file simply means built-in defaults are used; `mil` does not create one automatically on first run. Mutable state lives separately in `~/.local/state/mil/` (`session.json` and `bookmarks.json`). Writes are atomic and, on Unix, created with mode `0600`.

JavaScript is **off by default** in Blitz mode; enable it with `[javascript] enabled = true` (or disable it for one run with `--no-js`). Neither switch affects `--engine cdp`, where JavaScript is always on. The search template is `search_engine = "https://www.bing.com/search?q={}"`, where `{}` is the URL-encoded query placeholder. Theme, keybinding, image, network and UI sections are documented in [docs/CONFIG.md](docs/CONFIG.md).

## Limitations

- **JavaScript is deliberately partial.** No network promises or continuations, no XHR, no `localStorage`, no `requestAnimationFrame`, no modules or external scripts, no complex selectors (descendants, pseudo-classes, attribute operators), no DOM creation helpers, and no event dispatch beyond `click()`. `fetch` is a placeholder that records a navigation and returns a no-op thenable. React/Vue-style SPAs are out of scope. Scripts have a 16 MiB QuickJS memory limit and a virtual timer budget of at most 16 callbacks / 1000 ms, but this is not a security sandbox.
- **CSS support is bounded by Blitz.** Strokes/borders, box-shadow, gradients, SVG and custom paints are ignored; clip layers are approximated; there is no blend compositing; `overflow: auto`, 3D transforms and filters are partially unsupported by Blitz itself.
- **Cell-grid rendering is lossy.** A terminal grid has no sub-cell precision or exact fonts. Text is placed by reading-order grapheme spans rather than pixel-perfect glyph positions, so absolute positioning, precise spacing and pixel-perfect CSS are out of scope.
- **Single-threaded by necessity.** `BaseDocument` is `!Send`, so the document, JS context and surface stay on one current-thread Tokio runtime. Network requests are still async.
- **Binary size.** Much smaller than Chromium, but Stylo, Taffy and Parley are linked in.
- **CDP mode is optional and heavy.** It requires an external `chrome-headless-shell`/Chromium binary and ~100–300 MiB per instance; the page is a snapshot, UI shortcuts may leak into the page, one engine serves all tabs, and `--no-js` is ignored.
- **No bot-challenge bypass.** Google Search from a datacenter IP still returns `/sorry/index` in CDP mode; CAPTCHAs and anti-bot gates are out of scope.
- **Not a general-purpose browser.** No extensions, PDFs, DRM, WebGL, video playback, service workers or media playback. Windows support is best-effort.
- **Work in progress.** Find highlighting, a full help screen, link focus rings, end-to-end form submission, scripted image policy overrides, benchmark results and per-OS release smoke tests are still open.

## Real-world surfing notes

`mil` reads the HTML a server sends, so usable sites in the default Blitz mode are mostly those with a real server-rendered page: Hacker News, Wikipedia, Bing search, and many documentation sites work reasonably. Google Search stopped shipping results in its HTML in 2025 and now shows a "No readable content" hint in `mil`; DuckDuckGo's HTML endpoint and some large sites serve bot challenges. The default search template uses Bing, which still returns static results. In `--engine cdp` mode Bing, Wikipedia, Hacker News and GitHub render with full JavaScript, but Google still presents a `/sorry/index` bot challenge from datacenter IPs — anti-bot detection, not a renderer limitation.

Linked CSS can be fetched with `MIL_LOAD_STYLESHEETS=1` **in Blitz mode only**, and it is off by default: Blitz 0.2 does not implement every selector modern sites use to hide menus, so applying their CSS can expose hidden content. Chrome loads stylesheets itself in CDP mode, so the variable has no effect there. `MIL_IMAGE_PROTOCOL=none` skips terminal graphics probing (useful in tmux/CI) with either engine.

## Testing

Unit and integration tests run with `cargo test --workspace`. The end-to-end suite starts `mil` on a real pseudo-terminal, serves fixture pages from a local HTTP server, and asserts against a small ANSI screen model — so it checks what a human would actually see, including key handling, scrolling, link activation, tabs, JS mutations, find, bookmarks and session restore:

```sh
cargo build --bin mil
python3 scripts/e2e.py target/debug/mil
```

The suite uses only the Python standard library and runs in CI, alongside the 133 Rust unit/integration tests. `mil-cdp` also has one ignored real-browser integration test (`cargo test -p mil-cdp -- --ignored --nocapture`), which needs a local `chrome-headless-shell` and network access. Set `MIL_IMAGE_PROTOCOL=none` to skip terminal graphics capability probes in headless environments.

## Status and roadmap

Phase status and detailed acceptance criteria are tracked in [docs/ROADMAP.md](docs/ROADMAP.md). Bug reports and feature requests are welcome in the [issue tracker](https://github.com/SaibaWaipu/mil/issues); see [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

Dual-licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
