#![forbid(unsafe_code)]
//! Honestly partial JavaScript support for `mil`.
//!
//! Blitz has no script engine. This crate runs inline `<script>` bodies in a
//! fresh [rquickjs] context against a small mirrored DOM, records the writes
//! the script makes, and returns a [`JsReport`]. [`apply_report`] (or
//! [`apply_report_with_skipped`]) applies that report to a real Blitz
//! [`BaseDocument`](blitz_dom::BaseDocument).
//!
//! # Architecture
//!
//! rquickjs values are `!Send` and tied to a context lifetime, so callbacks
//! never capture a `&mut BaseDocument`. Instead:
//!
//! 1. The HTML is parsed once with `blitz-html` to build a plain, serialisable
//!    [`JsDomMirror`] whose node ids are the real Blitz node ids.
//! 2. Globals backed by `Rc<RefCell<JsDomMirror>>` are installed in a fresh
//!    context (one context per [`JsEngine::run`] call).
//! 3. Writes are collected in the mirror as [`DomMutation`]s, navigations,
//!    logs and errors.
//! 4. The caller applies the returned report; unapplicable mutations are
//!    skipped and reported instead of failing the page load.
//!
//! # Supported JavaScript surface
//!
//! - `document.title`, `document.cookie` (get/set; cookies are mirror-only),
//!   `document.write(string)`, `document.getElementById`,
//!   `document.querySelector`, `document.querySelectorAll` (array-like with
//!   `length`, numeric indices and `forEach`).
//! - Selectors are limited to `#id`, `.class`, a bare tag name, `a[href]` and
//!   `*`, optionally comma-separated.
//! - Elements support `textContent`, `innerHTML`, `style.display`,
//!   `style.color`, `style.setProperty`, `classList.add/remove/contains`,
//!   `getAttribute`/`setAttribute`, `value`, `addEventListener` and `click`.
//!   `click()` on an `<a href>` records a navigation and fires registered
//!   click listeners.
//! - `window.location.href` (get/set), `window.location.assign`, `window.alert`,
//!   `console.log/warn/error`, `setTimeout`, `setInterval`, `fetch` (records a
//!   navigation and returns a placeholder with a no-op `then`/`catch`) and
//!   `navigator.userAgent`.
//!
//! `setTimeout`/`setInterval` use a virtual clock: at most 16 callbacks run,
//! covering at most 1000 ms of virtual time (and at most the configured
//! [`JsEngine::with_timeout`] budget). Timers after that are discarded.
//!
//! Script errors are caught and appended to [`JsReport::errors`]; later scripts
//! still run.
//!
//! # Not supported
//!
//! No network access, no real cookie jar, no promise continuations, no
//! `XMLHttpRequest`, no `localStorage`, no `requestAnimationFrame`, no modules,
//! no external script loading, no `eval` changes, no complex CSS selectors
//! (descendants, pseudo-classes, attribute operators), no DOM creation helpers
//! (`createElement`, `appendChild`), no event dispatch beyond `click()`, no
//! layout-dependent APIs, and no streaming `document.write` semantics (its
//! accumulated text becomes one `SetHtml` mutation on `<body>` that replaces
//! the body's children when applied). Scripts are untrusted-ish
//! input: a 16 MiB QuickJS memory limit and bounded timer loop are enforced,
//! but this is not a security sandbox against hostile code.
//!
//! `innerHTML` getter returns *escaped mirror text*, not a serialised subtree;
//! the setter records a [`DomMutation::SetHtml`]. Applying that mutation with
//! markup requires an HTML parser provider (installed automatically by
//! [`apply_report`]); plain-text writes work without one.

use std::{cell::RefCell, rc::Rc};

use anyhow::{Result, bail};
use rquickjs::{CatchResultExt, Context, Runtime, Value};

mod apply;
mod globals;
mod mirror;
mod timers;

pub use apply::{apply_report, apply_report_with_skipped};
pub use mirror::{JsDomMirror, MirrorNode, Timer};

/// Upper bound on virtual timer time, in milliseconds.
pub const MAX_TOTAL_MS: u64 = timers::MAX_TOTAL_MS;
/// Upper bound on the number of timer callbacks executed per run.
pub const MAX_TIMER_ITERATIONS: usize = timers::MAX_TIMER_ITERATIONS;

/// The outcome of running one batch of scripts.
#[derive(Debug, Default)]
pub struct JsReport {
    /// New `document.title`, when a script assigned it.
    pub title: Option<String>,
    /// DOM changes to apply to the real document.
    pub mutations: Vec<DomMutation>,
    /// URLs that a script tried to navigate to (absolute, when resolvable).
    pub navigations: Vec<String>,
    /// `console` and `alert` output.
    pub logs: Vec<String>,
    /// Script errors, one per failed script or callback.
    pub errors: Vec<String>,
    /// Mutations the caller chose not to apply.
    ///
    /// [`apply_report`] only warns via `tracing`; use
    /// [`apply_report_with_skipped`] to collect reasons here.
    pub skipped: Vec<String>,
}

/// A single DOM write requested by JavaScript.
#[derive(Debug, Clone)]
pub enum DomMutation {
    /// Replace an element's children with a text node (or set a text node).
    SetText {
        /// Target node id in the Blitz document.
        node_id: usize,
        /// New text content.
        text: String,
    },
    /// Set one inline style property.
    SetStyle {
        /// Target element id.
        node_id: usize,
        /// CSS property name, kebab-case.
        property: String,
        /// CSS property value.
        value: String,
    },
    /// Set an HTML attribute.
    SetAttribute {
        /// Target element id.
        node_id: usize,
        /// Attribute name.
        name: String,
        /// Attribute value.
        value: String,
    },
    /// Replace an element's children with parsed HTML.
    SetHtml {
        /// Target element id.
        node_id: usize,
        /// Markup to parse.
        html: String,
    },
    /// Hide an element (`display: none`).
    Hide {
        /// Target element id.
        node_id: usize,
    },
}

impl DomMutation {
    /// Id of the node this mutation targets.
    pub fn node_id(&self) -> usize {
        match self {
            Self::SetText { node_id, .. }
            | Self::SetStyle { node_id, .. }
            | Self::SetAttribute { node_id, .. }
            | Self::SetHtml { node_id, .. }
            | Self::Hide { node_id } => *node_id,
        }
    }
}

/// A reusable JavaScript engine configuration.
///
/// Each [`run`](Self::run) creates a fresh runtime and context, so scripts from
/// different pages cannot share state.
#[derive(Debug, Clone, Copy)]
pub struct JsEngine {
    timeout_ms: u64,
}

impl JsEngine {
    /// Create an engine with the default 1000 ms virtual timer budget.
    pub fn new() -> Result<Self> {
        Ok(Self::with_timeout(MAX_TOTAL_MS))
    }

    /// Create an engine with a custom virtual timer budget, capped at 1000 ms.
    pub fn with_timeout(timeout_ms: u64) -> Self {
        Self { timeout_ms }
    }

    /// Run `scripts` against a mirror of `html` and return their mutations.
    ///
    /// `base_url` is used to resolve relative navigations (`<a href>`,
    /// `window.location` and `fetch`). A malformed or throwing script is
    /// recorded in [`JsReport::errors`] and does not abort the batch. Only a
    /// failure to parse the HTML or initialise the JS runtime is returned as an
    /// error.
    pub fn run(&mut self, html: &str, scripts: &[String], base_url: &str) -> Result<JsReport> {
        let mirror = JsDomMirror::from_html(html, base_url)?;
        let state = Rc::new(RefCell::new(mirror));
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(16 * 1024 * 1024);
        let context = Context::full(&runtime)?;
        let budget = self.timeout_ms;

        let result = context.with(|ctx| -> rquickjs::Result<JsReport> {
            globals::install(&ctx, &state)?;

            for script in scripts {
                if let Err(error) = ctx.eval::<Value, _>(script.as_str()).catch(&ctx) {
                    let message = error.to_string();
                    state.borrow_mut().error(format!("script error: {message}"));
                }
            }

            timers::run_timers(&ctx, &state, budget);

            let mut mirror = state.borrow_mut();
            mirror.flush_writes();
            // Saved JS values must not outlive their context.
            mirror.timers.clear();
            mirror.listeners.clear();
            Ok(std::mem::take(&mut mirror.report))
        });

        match result {
            Ok(report) => Ok(report),
            Err(error) => bail!("failed to install JavaScript globals: {error}"),
        }
    }
}

impl Default for JsEngine {
    fn default() -> Self {
        Self::with_timeout(MAX_TOTAL_MS)
    }
}
