# Contributing to mil

Thanks for your interest in `mil`! This document covers development setup, the workspace layout, coding conventions, testing, and the contribution workflow.

## Development setup

You need:

- A recent stable Rust toolchain (install via [rustup](https://rustup.rs/)): `rustup toolchain install stable`
- A terminal emulator with a decent Unicode font. For inline images, iTerm2, Kitty, or a Sixel-capable terminal is useful, but not required for development.
- On Linux: typical build dependencies for a Rust project (a C compiler, `pkg-config`, and OpenSSL headers if your platform needs them for TLS).

Clone and build:

```sh
git clone https://github.com/SaibaWaipu/mil.git
cd mil
cargo build --workspace
cargo run -p mil-app -- https://example.com
```

Configuration is read from `~/.config/mil/config.toml`; runtime state is written to `~/.local/state/mil/`. Use a dedicated config directory if you want to keep development state isolated from your daily browsing.

## Workspace layout

`mil` is a Cargo workspace split into focused crates:

| Crate | Kind | Responsibility |
| --- | --- | --- |
| `mil-app` | binary | The `mil` command: CLI parsing, wiring, and the main event loop |
| `mil-net` | library | HTTP(S) fetching, redirects, cookies on the wire, and response handling |
| `mil-dom` | library | DOM wrapper around Blitz (`blitz-dom`, `blitz-html`); link extraction, form discovery, history/snapshots |
| `mil-paint` | library | Painting the laid-out document into a terminal cell grid; image protocol output (iTerm2 OSC1337, Sixel, Kitty) |
| `mil-tui` | library | ratatui/crossterm widgets, keybindings, URL bar, tab bar, find bar, help screen |
| `mil-js` | library | Optional, partial `rquickjs` integration; a small, explicit subset of DOM/BOM APIs |
| `mil-session` | library | Session restore, history persistence, and bookmarks under the XDG state directory |

Keep dependencies pointing inward where possible: `mil-app` may depend on all crates, but library crates should avoid depending on each other unnecessarily. In particular, keep `mil-js` optional so the core build does not require a JavaScript engine.

### Important: Blitz is pinned exactly

Blitz 0.2.x is **beta** and its API changes between releases. All Blitz dependencies (`blitz-dom`, `blitz-html`, `blitz-paint`, and any related crates) are pinned to **exact versions** — no caret ranges. Do not relax or bump these pins casually. Upgrading them is a deliberate change that must include:

1. A full workspace build and test run.
2. A visual check that layout and painting still behave as expected.
3. A commit message explaining the version change and any API adaptations.

Also note that Blitz has no JavaScript support and `BaseDocument` is `!Send`; that is why the event loop uses a current-thread Tokio runtime. Preserve this constraint unless the upstream situation changes.

## Coding style

- Always run `cargo fmt --all` before committing. Formatting is enforced in CI.
- Run `cargo clippy --workspace --all-targets -- -D warnings` and fix all warnings. Do not add `#[allow(...)]` suppressions without a comment explaining why.
- Prefer small, focused functions and descriptive names. Public items should have doc comments.
- Handle errors explicitly — use `anyhow`/`thiserror` style results rather than `unwrap()` in library code. `expect()` is acceptable only where failure is a programmer bug.
- Keep the JavaScript surface in `mil-js` deliberately small. Adding a new API requires a test and a note in the README's feature list.
- Add tests for new behavior, especially in `mil-net`, `mil-dom`, and `mil-session`, where logic can be tested without a terminal.

## Tests

```sh
# Entire workspace
cargo test --workspace

# A single crate
cargo test -p mil-dom

# Formatting and lints
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Tests that need network access should be marked `#[ignore]` by default or use local fixtures/mock servers so the suite remains deterministic and offline-friendly.

## Commit convention

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

Common types: `feat` (new feature), `fix` (bug fix), `docs`, `refactor`, `test`, `chore`, `perf`, `build`, `ci`.

Examples:

```
feat(tui): add link cycling with Tab and Shift-Tab
fix(net): follow relative redirects against the request URL
docs: document workspace layout and pinned Blitz versions
```

Breaking changes use `!` after the type/scope (e.g. `feat(dom)!: change snapshot API`) and include a `BREAKING CHANGE:` footer.

## Pull request process

1. Fork the repository and create a topic branch from `main`.
2. Make focused changes; one PR per concern.
3. Ensure `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` all pass.
4. Update documentation when behavior changes (README, this file, and `CHANGELOG.md`).
5. Open a PR against `main` with a clear description: what changed, why, how you tested it, and any known limitations. Link related issues.
6. Address review feedback; keep commits clean (rebase/squash if requested).
7. A maintainer will merge when the PR is approved and CI is green.

By contributing, you agree that your contributions are licensed under the same dual MIT/Apache-2.0 terms as the project (see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)).

## Reporting bugs

Open an issue at <https://github.com/SaibaWaipu/mil/issues> and include:

- `mil --version` and your OS/terminal emulator
- The URL (or a minimal local reproduction)
- What you expected and what happened
- Relevant config and logs, with secrets redacted
