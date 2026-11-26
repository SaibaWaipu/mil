//! Optional, full-JavaScript backend for `mil` driven by Chrome over CDP.
//!
//! The default Blitz backend stays low-memory and fast; this crate is a
//! *separate, opt-in* engine for pages that need real JavaScript. It launches a
//! `chrome-headless-shell` process (or another Chromium-compatible binary),
//! lets Chrome do layout and scripting, walks the resulting DOM with a small
//! JavaScript snippet, and converts the measured text and image rectangles
//! into the same [`mil_paint::Surface`] the rest of `mil` renders.
//!
//! # Costs and caveats
//!
//! A full browser is heavy: expect roughly 100–300 MiB of resident memory for
//! a single instance, plus a child process and a websocket handler task. The
//! handler task must be polled continuously or every CDP command will time
//! out; [`CdpEngine`] owns that task for you. Always call [`CdpEngine::close`]
//! when you are done so the child is reaped cleanly. Because Chrome does the
//! rendering, colours and text layout match a real browser much more closely
//! than the default backend, at the cost of memory and startup latency.
//!
//! The DOM is sampled as a snapshot on each navigation/reload; live DOM
//! mutations after load are not pushed to the terminal until the next
//! snapshot is requested.

#![forbid(unsafe_code)]

mod color;
mod input;
mod js;
mod paint;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use chromiumoxide::Browser;
use chromiumoxide::Page;
use chromiumoxide::browser::BrowserConfig;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    GetNavigationHistoryParams, NavigateToHistoryEntryParams, Viewport,
};
use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;
use chromiumoxide::page::ScreenshotParams;
use crossterm::event::{Event, KeyCode};
use futures::StreamExt;
use mil_paint::{PlacedImage, Surface};
use tokio::task::JoinHandle;
use url::Url;

/// Configuration for the CDP backend.
#[derive(Debug, Clone)]
pub struct CdpConfig {
    /// Browser executable. `None` auto-detects Chrome/Chromium; `MIL_CHROME`
    /// overrides detection.
    pub executable: Option<PathBuf>,
    /// Directory for the browser profile. A unique directory in the system
    /// temp folder is used when this is `None`.
    pub user_data_dir: Option<PathBuf>,
    /// Horizontal pixels per terminal cell. Defaults to 8.
    pub cell_width: u32,
    /// Vertical pixels per terminal cell. Defaults to 16.
    pub cell_height: u32,
}

impl Default for CdpConfig {
    fn default() -> Self {
        Self {
            executable: None,
            user_data_dir: None,
            cell_width: 8,
            cell_height: 16,
        }
    }
}

/// One measured text run or image from the DOM walker.
///
/// Coordinates are in document pixels (viewport position plus scroll offset).
/// `text` is set for text runs, `src`/`alt` for images, and `href` when the run
/// is inside an anchor.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CdpSpan {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub alt: Option<String>,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub w: f64,
    #[serde(default)]
    pub h: f64,
    #[serde(default)]
    pub fg: Option<String>,
    #[serde(default)]
    pub bg: Option<String>,
    #[serde(default)]
    pub bold: Option<bool>,
    #[serde(default)]
    pub italic: Option<bool>,
    #[serde(default)]
    pub underline: Option<bool>,
    #[serde(default)]
    pub href: Option<String>,
}

/// A clickable region in document pixel coordinates.
#[derive(Debug, Clone)]
pub struct CdpLink {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub url: String,
}

/// A snapshot of one page, ready for the existing terminal renderer.
#[derive(Debug)]
pub struct CdpPage {
    pub url: Url,
    pub title: String,
    pub surface: Surface,
    pub links: Vec<CdpLink>,
    pub document_width: u16,
    pub document_height: u16,
}

/// Owns the browser process, page, and the CDP handler task.
///
/// Construct with [`CdpEngine::new`], drive input with [`CdpEngine::key_event`],
/// [`CdpEngine::click`], and [`CdpEngine::scroll`], and finish with
/// [`CdpEngine::close`]. Dropping the engine kills the child best-effort.
pub struct CdpEngine {
    browser: Option<Browser>,
    page: Page,
    handler: Option<JoinHandle<()>>,
    config: CdpConfig,
    scroll_x: f64,
    scroll_y: f64,
    mouse_x: f64,
    mouse_y: f64,
}

/// Commands from PATH tried after the explicit candidates in
/// [`CHROME_CANDIDATES`].
const PATH_COMMANDS: &[&str] = &[
    "chrome-headless-shell",
    "chromium",
    "google-chrome",
    "chrome",
];

/// Well-known Chromium locations, including the cached headless shell used by
/// the integration test in this repository.
const CHROME_CANDIDATES: &[&str] = &[
    "/home/waipu/.cache/puppeteer/chrome-headless-shell/linux-155.0.8059.79/chrome-headless-shell-linux64/chrome-headless-shell",
    "/usr/bin/chromium",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium-browser",
    "/opt/google/chrome/chrome",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
];

/// Locate a Chrome-compatible executable.
///
/// Order: `MIL_CHROME` (if set), well-known absolute paths, then commands on
/// `PATH`. A bad `MIL_CHROME` value is reported rather than silently ignored.
pub fn detect_executable() -> anyhow::Result<PathBuf> {
    let override_path = std::env::var_os("MIL_CHROME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    resolve_executable(override_path.as_deref(), CHROME_CANDIDATES, find_on_path)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_executable(
    override_path: Option<&Path>,
    candidates: &[&str],
    find: impl Fn(&str) -> Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!(
            "MIL_CHROME is set to {} but that file does not exist",
            path.display()
        );
    }
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
    }
    for command in PATH_COMMANDS {
        if let Some(path) = find(command) {
            return Ok(path);
        }
    }
    bail!(
        "no Chrome/Chromium executable found; set MIL_CHROME to the path of \\
         chrome-headless-shell or chromium (tried: {} and {} on PATH)",
        CHROME_CANDIDATES.join(", "),
        PATH_COMMANDS.join(", ")
    )
}

fn temp_profile_dir() -> anyhow::Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(std::env::temp_dir().join(format!("mil-cdp-{}-{now}", std::process::id())))
}

fn builder_error(message: String) -> anyhow::Error {
    anyhow::Error::msg(message)
}

/// Draw an image's alt text only when the painter decided it is safe: a blank
/// rectangle of at least 4x2 cells. Otherwise the reserved cells stay empty so
/// real text can never be overwritten by a label such as `[image]`.
fn fallback_alt(surface: &mut Surface, region: &paint::ImageRegion) {
    if region.alt.is_empty() {
        return;
    }
    paint::draw_alt(
        surface,
        region.cx,
        region.cy,
        region.cw,
        region.ch,
        &region.alt,
        mil_paint::CellStyle::new(),
    );
}

impl CdpEngine {
    /// Launch a browser and open a blank page.
    ///
    /// A failed launch aborts the handler task and kills the child process, so
    /// no browser is left running behind an error.
    pub async fn new(config: CdpConfig) -> anyhow::Result<Self> {
        let config = CdpConfig {
            cell_width: config.cell_width.max(1),
            cell_height: config.cell_height.max(1),
            ..config
        };
        let executable = match config.executable.as_deref() {
            Some(path) => path.to_path_buf(),
            None => detect_executable()?,
        };
        if !executable.is_file() {
            bail!("Chrome executable {} does not exist", executable.display());
        }
        let user_data_dir = match config.user_data_dir.as_deref() {
            Some(path) => path.to_path_buf(),
            None => temp_profile_dir()?,
        };
        tokio::fs::create_dir_all(&user_data_dir)
            .await
            .with_context(|| format!("create user data dir {}", user_data_dir.display()))?;

        let launch_config = BrowserConfig::builder()
            .chrome_executable(&executable)
            .no_sandbox()
            .arg("--disable-gpu")
            .user_data_dir(&user_data_dir)
            .port(0)
            .build()
            .map_err(builder_error)?;
        let (mut browser, mut handler) = Browser::launch(launch_config)
            .await
            .context("launch Chrome over CDP")?;

        // The handler must be polled continuously: without this task every CDP
        // command times out because no responses are being routed.
        let handler_task = tokio::spawn(async move {
            while let Some(result) = handler.next().await {
                if let Err(error) = result {
                    tracing::warn!(%error, "CDP handler event failed");
                }
            }
        });

        let page = match browser.new_page("about:blank").await {
            Ok(page) => page,
            Err(error) => {
                handler_task.abort();
                let _ = browser.kill().await;
                return Err(error).context("open initial page");
            }
        };

        Ok(Self {
            browser: Some(browser),
            page,
            handler: Some(handler_task),
            config,
            scroll_x: 0.0,
            scroll_y: 0.0,
            mouse_x: 0.0,
            mouse_y: 0.0,
        })
    }

    /// Emulate a viewport of `cols` x `rows` terminal cells.
    pub async fn set_viewport(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let width = u32::from(cols)
            .saturating_mul(self.config.cell_width)
            .clamp(1, 10_000);
        let height = u32::from(rows)
            .saturating_mul(self.config.cell_height)
            .clamp(1, 10_000);
        let params = SetDeviceMetricsOverrideParams::builder()
            .width(i64::from(width))
            .height(i64::from(height))
            .device_scale_factor(1.0)
            .mobile(false)
            .build()
            .map_err(builder_error)?;
        self.page
            .execute(params)
            .await
            .context("set device metrics")?;
        self.mouse_x = f64::from(width) / 2.0;
        self.mouse_y = f64::from(height) / 2.0;
        Ok(())
    }

    /// Navigate to `url`, wait for load, and sample the rendered page.
    pub async fn navigate(&mut self, url: &Url, cols: u16, rows: u16) -> anyhow::Result<CdpPage> {
        self.set_viewport(cols, rows).await?;
        self.page
            .goto(url.as_str())
            .await
            .with_context(|| format!("navigate to {url}"))?;
        self.capture(cols, rows).await
    }

    /// Re-sample the current page without reloading it.
    ///
    /// Useful after in-page interactions (typing, clicking a button) that do
    /// not trigger a navigation.
    pub async fn snapshot(&mut self, cols: u16, rows: u16) -> anyhow::Result<CdpPage> {
        self.set_viewport(cols, rows).await?;
        self.capture(cols, rows).await
    }

    /// Reload the current page and sample it again.
    pub async fn reload(&mut self, cols: u16, rows: u16) -> anyhow::Result<CdpPage> {
        self.set_viewport(cols, rows).await?;
        self.page.reload().await.context("reload page")?;
        self.capture(cols, rows).await
    }

    /// Move one entry back in session history, then sample the page.
    pub async fn back(&mut self, cols: u16, rows: u16) -> anyhow::Result<CdpPage> {
        self.history_step(-1, cols, rows).await
    }

    /// Move one entry forward in session history, then sample the page.
    pub async fn forward(&mut self, cols: u16, rows: u16) -> anyhow::Result<CdpPage> {
        self.history_step(1, cols, rows).await
    }

    async fn history_step(&mut self, offset: i64, cols: u16, rows: u16) -> anyhow::Result<CdpPage> {
        self.set_viewport(cols, rows).await?;
        let history = self
            .page
            .execute(GetNavigationHistoryParams::default())
            .await
            .context("read navigation history")?
            .result;
        let target = history.current_index.saturating_add(offset);
        if target >= 0 && (target as usize) < history.entries.len() {
            let entry_id = history.entries[target as usize].id;
            self.page
                .execute(NavigateToHistoryEntryParams::new(entry_id))
                .await
                .context("navigate in history")?;
            self.wait_for_document_ready().await?;
        }
        self.capture(cols, rows).await
    }

    async fn wait_for_document_ready(&self) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        loop {
            let params = EvaluateParams::builder()
                .expression("document.readyState")
                .return_by_value(true)
                .build()
                .map_err(builder_error)?;
            let state = self
                .page
                .evaluate(params)
                .await
                .and_then(|result| Ok(result.into_value::<String>()?));
            match state {
                Ok(state) if state == "complete" || state == "interactive" => return Ok(()),
                Ok(_) | Err(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Forward a crossterm key event to the page.
    ///
    /// Keys the surrounding TUI owns (quit, address bar, back) and release
    /// events are ignored. Printable characters use `Input.insertText`;
    /// navigation/editing keys are sent as key down + key up pairs.
    pub async fn key_event(&mut self, event: &Event) -> anyhow::Result<()> {
        if input::is_ui_event(event) {
            return Ok(());
        }
        let Event::Key(key) = event else {
            return Ok(());
        };
        let modifiers = input::cdp_modifiers(key.modifiers);

        if let KeyCode::Char(ch) = key.code {
            if input::is_insertable(key.code, key.modifiers) {
                let params = InsertTextParams::builder()
                    .text(ch.to_string())
                    .build()
                    .map_err(builder_error)?;
                self.page.execute(params).await.context("insert text")?;
                return Ok(());
            }
        }

        let Some(definition) = input::key_definition(key.code) else {
            return Ok(());
        };
        for kind in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let params = DispatchKeyEventParams::builder()
                .r#type(kind)
                .key(definition.key.clone())
                .code(definition.code.clone())
                .windows_virtual_key_code(definition.virtual_key)
                .modifiers(modifiers)
                .build()
                .map_err(builder_error)?;
            self.page
                .execute(params)
                .await
                .context("dispatch key event")?;
        }
        Ok(())
    }

    /// Click at *document* pixel coordinates (scroll offset is subtracted
    /// before dispatching the viewport-relative CDP mouse event).
    pub async fn click(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
        let view_x = x - self.scroll_x;
        let view_y = y - self.scroll_y;
        self.mouse_x = view_x;
        self.mouse_y = view_y;

        let moved = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(view_x)
            .y(view_y)
            .button(MouseButton::None)
            .build()
            .map_err(builder_error)?;
        self.page.execute(moved).await.context("move mouse")?;

        let pressed = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(view_x)
            .y(view_y)
            .button(MouseButton::Left)
            .buttons(1)
            .click_count(1)
            .build()
            .map_err(builder_error)?;
        self.page.execute(pressed).await.context("press mouse")?;

        let released = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(view_x)
            .y(view_y)
            .button(MouseButton::Left)
            .buttons(0)
            .click_count(1)
            .build()
            .map_err(builder_error)?;
        self.page.execute(released).await.context("release mouse")?;
        Ok(())
    }

    /// Scroll the page vertically by `dy` CSS pixels (positive scrolls down).
    pub async fn scroll(&mut self, dy: f64) -> anyhow::Result<()> {
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseWheel)
            .x(self.mouse_x)
            .y(self.mouse_y)
            .button(MouseButton::None)
            .delta_x(0.0)
            .delta_y(dy)
            .build()
            .map_err(builder_error)?;
        self.page.execute(params).await.context("scroll page")?;
        self.scroll_y = (self.scroll_y + dy).max(0.0);
        Ok(())
    }

    /// Close the browser and reap the child process.
    pub async fn close(&mut self) -> anyhow::Result<()> {
        let Some(mut browser) = self.browser.take() else {
            return Ok(());
        };
        if let Err(error) = browser.close().await {
            // Keep the browser so Drop can still kill it.
            self.browser = Some(browser);
            return Err(error).context("close browser");
        }
        browser.wait().await.context("wait for browser exit")?;
        if let Some(handler) = self.handler.take() {
            handler.abort();
        }
        Ok(())
    }

    async fn capture(&mut self, cols: u16, rows: u16) -> anyhow::Result<CdpPage> {
        // Send the raw CDP command rather than `Page::evaluate`, which pins an
        // execution context id that can go stale immediately after a
        // navigation. Without a context id Chrome uses the page's current one.
        let params = EvaluateParams::builder()
            .expression(js::EXTRACT_JS)
            .return_by_value(true)
            .await_promise(true)
            .build()
            .map_err(builder_error)?;
        let response = self
            .page
            .execute(params)
            .await
            .context("evaluate DOM walker")?;
        let evaluated = response.result;
        if let Some(details) = evaluated.exception_details {
            bail!("DOM walker threw: {}", details.text);
        }
        let value = evaluated
            .result
            .value
            .ok_or_else(|| anyhow::anyhow!("DOM walker returned no value"))?;
        let encoded: String = serde_json::from_value(value).context("decode DOM walker result")?;
        let extracted: js::Extracted =
            serde_json::from_str(&encoded).context("parse DOM walker JSON")?;

        self.scroll_x = extracted.scroll_x.max(0.0);
        self.scroll_y = extracted.scroll_y.max(0.0);

        let document_width = paint::cells_for(extracted.width, self.config.cell_width);
        let document_height =
            paint::surface_height(extracted.height, rows, self.config.cell_height);
        // The surface is document-sized (like the Blitz one); the TUI scrolls a
        // viewport window over it.
        let painted = paint::paint_spans(&extracted.spans, &self.config, cols, document_height);
        let mut surface = painted.surface;
        let mut image_id = 1u64;
        for region in painted.images {
            let clip = match Viewport::builder()
                .x(region.x.round().max(0.0))
                .y(region.y.round().max(0.0))
                .width(region.w.round().clamp(1.0, 4096.0))
                .height(region.h.round().clamp(1.0, 4096.0))
                .scale(1.0)
                .build()
                .map_err(builder_error)
            {
                Ok(clip) => clip,
                Err(error) => {
                    tracing::warn!(%error, "skip image with invalid clip");
                    continue;
                }
            };
            let screenshot = self
                .page
                .screenshot(
                    ScreenshotParams::builder()
                        .clip(clip)
                        .capture_beyond_viewport(true)
                        .build(),
                )
                .await;
            match screenshot {
                Ok(bytes) if !bytes.is_empty() => {
                    // The TUI substitutes "[image]" for an empty alt label on
                    // terminals without inline graphics. Decorative images
                    // (logos, spacers) have no alt text, so give them a blank
                    // label instead of spraying "[image]" over the page.
                    let label = if region.alt.is_empty() {
                        " ".to_owned()
                    } else {
                        region.alt.clone()
                    };
                    surface.push_image(PlacedImage::new(
                        image_id, region.cx, region.cy, region.cw, region.ch, bytes, label,
                    ));
                }
                Ok(_) => {
                    tracing::warn!("empty screenshot for image {image_id}");
                    fallback_alt(&mut surface, &region);
                }
                Err(error) => {
                    tracing::warn!(%error, "screenshot for image {image_id} failed");
                    fallback_alt(&mut surface, &region);
                }
            }
            image_id += 1;
        }

        let url = match Url::parse(&extracted.url) {
            Ok(url) => url,
            Err(_) => Url::parse("about:blank").context("fallback URL")?,
        };
        Ok(CdpPage {
            url,
            title: extracted.title,
            surface,
            links: painted.links,
            document_width,
            document_height,
        })
    }
}

impl Drop for CdpEngine {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            handler.abort();
        }
        if let Some(browser) = self.browser.take() {
            // No handler is needed for `kill`, so this works even though the
            // handler task was just aborted. If there is no runtime, Chrome's
            // own `kill_on_drop` behaviour cleans up the child.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let mut browser = browser;
                    let _ = browser.kill().await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn existing_file() -> PathBuf {
        std::env::current_exe().expect("test binary has a path")
    }

    #[test]
    fn executable_override_wins() {
        let exe = existing_file();
        let found = resolve_executable(Some(&exe), &[], |_| None).expect("override resolves");
        assert_eq!(found, exe);
    }

    #[test]
    fn bad_override_is_reported() {
        let error = resolve_executable(Some(Path::new("/no/such/chrome")), &[], |_| None)
            .expect_err("bad override fails");
        assert!(error.to_string().contains("MIL_CHROME"));
    }

    #[test]
    fn falls_back_to_known_candidates() {
        let exe = existing_file();
        let candidate = exe.to_string_lossy().into_owned();
        let found = resolve_executable(None, &["/no/such/chrome", &candidate], |_| None)
            .expect("candidate resolves");
        assert_eq!(found, exe);
    }

    #[test]
    fn falls_back_to_path_lookup() {
        let exe = existing_file();
        let found = resolve_executable(None, &[], |name| {
            (name == "chrome-headless-shell").then(|| exe.clone())
        })
        .expect("PATH resolves");
        assert_eq!(found, exe);
    }

    #[test]
    fn missing_executable_error_mentions_env_var() {
        let error = resolve_executable(None, &[], |_| None).expect_err("missing fails");
        let message = error.to_string();
        assert!(message.contains("MIL_CHROME"), "{message}");
        assert!(message.contains("chrome-headless-shell"), "{message}");
    }

    #[test]
    fn default_config_matches_cell_geometry() {
        let config = CdpConfig::default();
        assert_eq!((config.cell_width, config.cell_height), (8, 16));
        assert!(config.executable.is_none());
        assert!(config.user_data_dir.is_none());
    }
}
