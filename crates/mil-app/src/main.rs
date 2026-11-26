//! `mil` — a fast, minimal terminal web browser.
//!
//! The event loop is intentionally single-threaded: Blitz documents are
//! `!Send`, and a browser tab is naturally a single-threaded state machine.

use std::{io, path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow};
use blitz_traits::shell::{ColorScheme, Viewport};
use clap::{Parser, ValueEnum};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use mil_cdp::{CdpConfig, CdpEngine, CdpLink, CdpPage};
use mil_dom::{LinkRegion, Page, PaintConfig, render_page};
use mil_js::{JsDomMirror, JsEngine};
use mil_net::{Client, NetConfig};
use mil_paint::Surface;
use mil_session::{Bookmarks, Config, Session, TabSnapshot};
use mil_tui::{Action, TabInfo, TerminalView, ViewState};
use tracing::{debug, warn};
use url::{Url, form_urlencoded};

const DEFAULT_HOME: &str = "https://example.com/";
const CELL_WIDTH_PX: u32 = 8;
const CELL_HEIGHT_PX: u32 = 16;
/// Upper bound on painted rows for one document, so a pathological page cannot
/// make the cell grid consume unbounded memory.
const MAX_SURFACE_ROWS: u16 = 1024;

/// Rendering backend. Blitz is the self-contained default; CDP drives an
/// external headless Chrome for sites that need real JavaScript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
enum EngineMode {
    /// Built-in Blitz layout engine (limited JavaScript support).
    #[default]
    Blitz,
    /// Chrome DevTools Protocol backend (full JavaScript support).
    Cdp,
}

/// A fast, minimal terminal web browser.
#[derive(Debug, Parser)]
#[command(name = "mil", version, about, long_about = None)]
struct Args {
    /// URL or search query to open on startup
    #[arg(value_name = "URL")]
    url: Option<String>,

    /// Use an alternate configuration file
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Do not restore the previous session
    #[arg(long)]
    no_session: bool,

    /// Disable JavaScript even if enabled in the config
    #[arg(long)]
    no_js: bool,

    /// Rendering engine to use
    #[arg(long, value_enum, default_value_t = EngineMode::Blitz)]
    engine: EngineMode,

    /// Path to the headless Chrome shell used by the CDP engine
    #[arg(long, value_name = "PATH")]
    chrome: Option<PathBuf>,

    /// Increase logging verbosity (repeatable)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.verbose);

    let mut config = match &args.config {
        Some(path) => {
            Config::load_from(path).with_context(|| format!("loading {}", path.display()))?
        }
        None => Config::load().context("loading configuration")?,
    };
    if args.no_js {
        config.javascript.enabled = false;
    }
    let client = Client::new(net_config(&config)).context("creating HTTP client")?;
    let bookmarks = Bookmarks::load().unwrap_or_else(|err| {
        warn!("could not load bookmarks: {err:#}");
        Bookmarks::default()
    });

    // The view probes the terminal for graphics capabilities by writing escape
    // sequences to stdout, so it must be created after entering the alternate
    // screen. `try_init` also lets us clean up a partially initialised
    // terminal instead of relying on `init`'s panic.
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = ratatui::try_restore();
            return Err(err).context("initialising terminal");
        }
    };
    let view = match TerminalView::new() {
        Ok(view) => view,
        Err(err) => {
            ratatui::restore();
            return Err(err).context("initialising terminal view");
        }
    };
    // The graphics-capability probe enables and then drops its own raw-mode
    // guard, which would put the terminal back into cooked mode. ratatui's
    // `try_init` already enabled raw mode, so restore it before the event loop.
    if let Err(err) = crossterm::terminal::enable_raw_mode() {
        ratatui::restore();
        return Err(err).context("re-enabling raw mode after terminal probe");
    }

    // Start the browser only after the terminal is in raw mode, so a missing
    // Chrome binary (or a failed handshake) produces a visible error instead
    // of leaving a half-initialised TUI behind.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let cdp = if args.engine == EngineMode::Cdp {
        let config = CdpConfig {
            executable: args.chrome.clone(),
            ..CdpConfig::default()
        };
        match CdpEngine::new(config).await {
            Ok(mut engine) => {
                if let Err(err) = engine.set_viewport(cols, rows).await {
                    let _ = engine.close().await;
                    ratatui::restore();
                    return Err(err).context("setting CDP viewport");
                }
                Some(engine)
            }
            Err(err) => {
                ratatui::restore();
                return Err(err).context("initialising CDP engine");
            }
        }
    } else {
        None
    };

    let mut app = App {
        config,
        client,
        view,
        tabs: Vec::new(),
        active: 0,
        bookmarks,
        restore_session: !args.no_session,
        focused_link: 0,
        engine_mode: args.engine,
        cdp,
    };

    let result = async {
        // A broken session or an unreachable start page must not prevent the
        // browser from starting: the user can still navigate elsewhere.
        if let Err(err) = app.bootstrap().await {
            warn!("could not restore the starting page: {err:#}");
        }
        enable_mouse_capture()?;
        if let Some(input) = args.url.as_deref() {
            let url = resolve_input(input, &app.config);
            app.navigate(url, true).await?;
        }
        app.run(&mut terminal).await
    }
    .await;

    // Cleanup must run on every exit path. In particular, a failure to close
    // the browser or disable mouse capture must not skip leaving raw mode /
    // the alternate screen.
    if let Some(cdp) = app.cdp.as_mut() {
        if let Err(err) = cdp.close().await {
            warn!("could not close CDP engine: {err:#}");
        }
    }
    if let Err(err) = disable_mouse_capture() {
        warn!("could not disable mouse capture: {err:#}");
    }
    ratatui::restore();

    if let Err(err) = app.save_session() {
        warn!("could not save session: {err:#}");
    }

    result
}

fn init_logging(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter =
        format!("mil={level},mil_tui={level},mil_net={level},mil_dom={level},mil_cdp={level}");
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}

fn net_config(config: &Config) -> NetConfig {
    NetConfig {
        timeout: Duration::from_secs(config.network.timeout_secs.max(1)),
        proxy: config.network.proxy.clone(),
        user_agent: config
            .user_agent
            .clone()
            .unwrap_or_else(|| NetConfig::default().user_agent),
        ..NetConfig::default()
    }
}

fn enable_mouse_capture() -> Result<()> {
    crossterm::execute!(io::stdout(), event::EnableMouseCapture)
        .context("enabling mouse capture")?;
    Ok(())
}

fn disable_mouse_capture() -> Result<()> {
    crossterm::execute!(io::stdout(), event::DisableMouseCapture)
        .context("disabling mouse capture")?;
    Ok(())
}

/// The URL used when the omnibox is empty and for new tabs.
fn homepage_url(config: &Config) -> Url {
    Url::parse(&config.homepage)
        .unwrap_or_else(|_| Url::parse(DEFAULT_HOME).expect("valid default URL"))
}

/// Resolve omnibox input into a URL: accept absolute URLs, bare hosts, and
/// fall back to the configured search engine template.
fn resolve_input(input: &str, config: &Config) -> Url {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return homepage_url(config);
    }

    if let Ok(url) = Url::parse(trimmed) {
        if matches!(url.scheme(), "http" | "https" | "file" | "data") {
            return url;
        }
    }

    if trimmed.contains('.') || trimmed.starts_with("localhost") || trimmed.starts_with('[') {
        let authority = trimmed.split(['/', '?', '#']).next().unwrap_or(trimmed);
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let hostname = if let Some(rest) = host.strip_prefix('[') {
            // Bracketed IPv6 literal, with an optional port after `]`.
            rest.split(']').next().unwrap_or(rest)
        } else {
            host.rsplit_once(':').map_or(host, |(name, _)| name)
        };
        // Local development servers speak plain HTTP; forcing HTTPS on them
        // makes `localhost:8000` and `127.0.0.1:8000` unusable.
        let local = host.starts_with('[')
            || hostname == "localhost"
            || hostname.starts_with("127.")
            || hostname.starts_with("10.")
            || hostname.starts_with("192.168.");
        let scheme = if local { "http" } else { "https" };
        if let Ok(url) = Url::parse(&format!("{scheme}://{trimmed}")) {
            return url;
        }
    }

    let query = form_urlencoded::byte_serialize(trimmed.as_bytes()).collect::<String>();
    let template = if config.search_engine.contains("{}") {
        config.search_engine.clone()
    } else {
        "https://www.bing.com/search?q={}".to_owned()
    };
    Url::parse(&template.replace("{}", &query))
        .unwrap_or_else(|_| Url::parse(&format!("{DEFAULT_HOME}?q={query}")).expect("valid URL"))
}

/// Keep a scroll position inside a finite, non-negative range.
fn clamp_scroll(tab: &mut Tab, value: f64) {
    let max = if tab.scroll_max_y.is_finite() && tab.scroll_max_y >= 0.0 {
        tab.scroll_max_y
    } else {
        0.0
    };
    tab.scroll_y = if value.is_finite() {
        value.clamp(0.0, max)
    } else {
        0.0
    };
}

/// Hit-test a link using the document-relative cell coordinates emitted by the
/// view. `row` already includes the scroll offset, so it must not be adjusted
/// again here.
fn link_at(links: &[LinkRegion], col: u16, row: u16) -> Option<&LinkRegion> {
    let x = (f32::from(col) + 0.5) * CELL_WIDTH_PX as f32;
    let y = (f32::from(row) + 0.5) * CELL_HEIGHT_PX as f32;
    links.iter().find(|link| {
        x >= link.x && x <= link.x + link.width && y >= link.y && y <= link.y + link.height
    })
}

/// First painted row whose text contains `query` (case-insensitive).
fn find_row(page: &Page, query: &str) -> Option<f64> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    page.text_runs().into_iter().find_map(|run| {
        run.text
            .to_lowercase()
            .contains(&needle)
            .then(|| (run.y / CELL_HEIGHT_PX as f32).floor().max(0.0) as f64)
    })
}

/// Case-insensitive search over a painted surface, used when there is no Blitz
/// `Page` to walk (CDP pages are painted by the engine's JS snapshotter).
fn find_row_in_surface(surface: &Surface, query: &str) -> Option<f64> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    (0..surface.height).find_map(|row| {
        let text: String = (0..surface.width)
            .filter_map(|col| surface.get(col, row))
            .filter(|cell| !cell.continuation)
            .map(|cell| cell.symbol.as_str())
            .collect();
        text.to_lowercase()
            .contains(&needle)
            .then(|| f64::from(row))
    })
}

/// Hit-test a CDP link using the document pixel coordinates reported by the
/// engine's JS walker.
fn cdp_link_at(links: &[CdpLink], x: f64, y: f64) -> Option<&CdpLink> {
    links
        .iter()
        .find(|link| x >= link.x && x <= link.x + link.w && y >= link.y && y <= link.y + link.h)
}

/// Number of painted rows needed to hold the document's content.
///
/// `render_page` writes into a surface of exactly the height it is given, so
/// passing just the viewport height would silently truncate every page below
/// the fold and make scrolling impossible.
fn surface_rows(page: &Page, viewport_rows: u16) -> u16 {
    let rows = viewport_rows.max(1);
    let height_px = page.doc.root_element().final_layout.size.height;
    if !height_px.is_finite() || height_px <= 0.0 {
        return rows;
    }
    let needed = (height_px / CELL_HEIGHT_PX as f32).ceil().max(0.0) as u32;
    // `clamp` would panic if a (pathologically tall) terminal had more rows
    // than the hard cap, so keep the viewport height as the lower bound.
    let cap = u32::from(rows)
        .saturating_mul(16)
        .min(u32::from(MAX_SURFACE_ROWS))
        .max(u32::from(rows));
    needed.clamp(u32::from(rows), cap) as u16
}

/// Engine-independent result of loading a page, ready to install in a tab.
struct LoadedPage {
    url: Url,
    title: String,
    /// Present only for Blitz pages; CDP pages have no Blitz document.
    page: Option<Page>,
    surface: Surface,
    links: Vec<CdpLink>,
}

impl From<CdpPage> for LoadedPage {
    fn from(page: CdpPage) -> Self {
        let CdpPage {
            url,
            title,
            surface,
            links,
            ..
        } = page;
        Self {
            url,
            title,
            page: None,
            surface,
            links,
        }
    }
}

struct Tab {
    url: Url,
    title: String,
    back: Vec<Url>,
    forward: Vec<Url>,
    page: Option<Page>,
    surface: Option<Surface>,
    /// Links collected by the CDP JS walker. `Page::links()` is unavailable
    /// for CDP pages, so hit-testing uses these document-pixel rectangles.
    cdp_links: Vec<CdpLink>,
    scroll_y: f64,
    scroll_max_y: f64,
    /// Last navigation failure, shown in the status line until the next load.
    error: Option<String>,
}

impl Tab {
    fn new(url: Url) -> Self {
        Self {
            title: url.host_str().unwrap_or("new tab").to_owned(),
            url,
            back: Vec::new(),
            forward: Vec::new(),
            page: None,
            surface: None,
            cdp_links: Vec::new(),
            scroll_y: 0.0,
            scroll_max_y: 0.0,
            error: None,
        }
    }

    fn snapshot(&self) -> TabSnapshot {
        TabSnapshot {
            url: self.url.to_string(),
            title: self.title.clone(),
            scroll_x: 0.0,
            scroll_y: self.scroll_y,
            back: self.back.iter().map(Url::to_string).collect(),
            forward: self.forward.iter().map(Url::to_string).collect(),
        }
    }

    fn from_snapshot(snap: &TabSnapshot) -> Option<Self> {
        let url = Url::parse(&snap.url).ok()?;
        let mut tab = Self::new(url);
        tab.title.clone_from(&snap.title);
        tab.scroll_y = if snap.scroll_y.is_finite() {
            snap.scroll_y.max(0.0)
        } else {
            0.0
        };
        tab.back = snap
            .back
            .iter()
            .filter_map(|s| Url::parse(s).ok())
            .collect();
        tab.forward = snap
            .forward
            .iter()
            .filter_map(|s| Url::parse(s).ok())
            .collect();
        Some(tab)
    }
}

struct App {
    config: Config,
    client: Client,
    view: TerminalView,
    tabs: Vec<Tab>,
    active: usize,
    bookmarks: Bookmarks,
    restore_session: bool,
    /// Index of the link that Enter will activate.
    focused_link: usize,
    engine_mode: EngineMode,
    /// Present only in CDP mode; a single engine drives the active tab.
    cdp: Option<CdpEngine>,
}

impl App {
    fn is_cdp(&self) -> bool {
        self.engine_mode == EngineMode::Cdp
    }

    fn cdp_engine(&mut self) -> Result<&mut CdpEngine> {
        self.cdp.as_mut().context("CDP engine is not running")
    }

    async fn bootstrap(&mut self) -> Result<()> {
        if self.restore_session {
            match Session::load() {
                Ok(session) if !session.tabs.is_empty() => {
                    self.tabs = session.tabs.iter().filter_map(Tab::from_snapshot).collect();
                    self.active = session.active.min(self.tabs.len().saturating_sub(1));
                }
                Ok(_) => {}
                Err(err) => warn!("could not restore session: {err:#}"),
            }
        }
        if self.tabs.is_empty() {
            self.tabs.push(Tab::new(homepage_url(&self.config)));
        }
        self.restore_active().await
    }

    /// Fetch the active tab's page if it has not been loaded yet.
    ///
    /// The tab's scroll position is kept across tab switches and session
    /// restore. History navigation clears it before calling this method.
    async fn restore_active(&mut self) -> Result<()> {
        let scroll = self.active_tab().scroll_y;
        let url = self.active_tab().url.clone();
        if self.active_tab().page.is_none() {
            self.navigate(url, false).await?;
            let tab = self.active_tab_mut();
            clamp_scroll(tab, scroll);
        }
        Ok(())
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn tab_infos(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .map(|tab| TabInfo {
                title: if tab.title.is_empty() {
                    tab.url.host_str().unwrap_or("about:blank").to_owned()
                } else {
                    tab.title.clone()
                },
                url: tab.url.to_string(),
            })
            .collect()
    }

    fn status_text(&self) -> String {
        if let Some(error) = &self.active_tab().error {
            return format!("Error: {error}");
        }
        if self.is_cdp() {
            "JS: full (CDP)  |  ? help".to_owned()
        } else if self.config.javascript.enabled {
            "JS: partial  |  ? help".to_owned()
        } else {
            "JS: off  |  ? help".to_owned()
        }
    }

    /// Navigate without ever terminating the browser because of a bad URL,
    /// a dead server or a paint failure. The failure is reported in the status
    /// line and the previous page (if any) stays available.
    async fn navigate(&mut self, url: Url, record_history: bool) -> Result<()> {
        match self.load(url, record_history).await {
            Ok(()) => Ok(()),
            Err(err) => {
                warn!("navigation failed: {err:#}");
                let message = format!("{err:#}").replace('\n', " ");
                self.active_tab_mut().error = Some(message.chars().take(160).collect());
                Ok(())
            }
        }
    }

    async fn load(&mut self, url: Url, record_history: bool) -> Result<()> {
        if self.is_cdp() {
            return self.load_cdp(url, record_history).await;
        }
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let (final_url, body) = if url.scheme() == "file" {
            // reqwest cannot fetch file URLs; read them locally so the URLs
            // accepted by `resolve_input` actually work.
            let path = url
                .to_file_path()
                .map_err(|_| anyhow!("file URL has no valid path: {url}"))?;
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("reading {}", path.display()))?;
            (url.clone(), bytes)
        } else {
            let response = self
                .client
                .get(&url)
                .await
                .with_context(|| format!("fetching {url}"))?;
            debug!(
                "{} {} ({} bytes)",
                response.status,
                response.url,
                response.body.len()
            );
            (response.url, response.body.to_vec())
        };

        let html = String::from_utf8_lossy(&body);
        let viewport = Viewport::new(
            u32::from(cols) * CELL_WIDTH_PX,
            u32::from(rows) * CELL_HEIGHT_PX,
            1.0,
            ColorScheme::Light,
        );
        let mut page = Page::from_html(&html, &final_url, viewport)
            .with_context(|| format!("parsing {}", final_url))?;
        self.load_stylesheets(&mut page).await;
        page.resolve(0.0);
        if self.config.javascript.enabled {
            self.run_scripts(&html, &mut page, &final_url);
        }
        let title = page.title().to_owned();

        let config = PaintConfig::default();
        let surface = render_page(&page, &config, cols, surface_rows(&page, rows))
            .with_context(|| format!("painting {}", final_url))?;
        self.install_page(
            LoadedPage {
                url: final_url,
                title,
                page: Some(page),
                surface,
                links: Vec::new(),
            },
            record_history,
            rows,
        );
        Ok(())
    }

    /// Apply a freshly painted page to the active tab. Both engines share the
    /// history, title, scroll and "no readable content" handling so switching
    /// `--engine` does not change the visible browser behaviour.
    fn install_page(&mut self, loaded: LoadedPage, record_history: bool, rows: u16) {
        let LoadedPage {
            url: final_url,
            title,
            page,
            surface,
            links: cdp_links,
        } = loaded;
        // The tab bar is only drawn with more than one tab; the URL and status
        // rows are always present.
        let chrome = if self.tabs.len() > 1 { 3 } else { 2 };
        let visible_rows = rows.saturating_sub(chrome).max(1);
        let scroll_max_y = surface.height.saturating_sub(visible_rows) as f64;
        let readable_cells = surface
            .cells()
            .iter()
            .filter(|cell| !cell.continuation && !cell.symbol.trim().is_empty())
            .count();
        let notice = if readable_cells < 8 {
            Some("No readable content - site likely requires JavaScript".to_owned())
        } else {
            None
        };

        let tab = self.active_tab_mut();
        if record_history && tab.url != final_url {
            tab.back.push(tab.url.clone());
            tab.forward.clear();
        }
        tab.url = final_url;
        tab.title = if title.is_empty() {
            tab.url.host_str().unwrap_or("untitled").to_owned()
        } else {
            title
        };
        tab.page = page;
        tab.surface = Some(surface);
        tab.cdp_links = cdp_links;
        tab.scroll_y = 0.0;
        tab.scroll_max_y = scroll_max_y.max(0.0);
        tab.error = notice;
        self.focused_link = 0;
    }

    /// CDP counterpart of `load`: the engine fetches, runs scripts and paints
    /// the page; the Blitz JS façade is deliberately not invoked here.
    async fn load_cdp(&mut self, url: Url, record_history: bool) -> Result<()> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        debug!("cdp navigate {url}");
        let page = self.cdp_engine()?.navigate(&url, cols, rows).await?;
        self.install_page(page.into(), record_history, rows);
        Ok(())
    }

    /// Re-read the CDP engine's current page after an interaction. Using
    /// `snapshot` rather than `reload` keeps in-page state (form values,
    /// application state) intact.
    async fn cdp_refresh_current(&mut self, record_history: bool) -> Result<()> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let page = self.cdp_engine()?.snapshot(cols, rows).await?;
        self.install_page(page.into(), record_history, rows);
        Ok(())
    }

    /// Dispatch a click on a recorded CDP link, then resynchronise the painted
    /// surface with whatever page the click landed on.
    async fn cdp_follow_link(&mut self, link: CdpLink) -> Result<()> {
        let x = link.x + link.w / 2.0;
        let y = link.y + link.h / 2.0;
        self.cdp_engine()?.click(x, y).await?;
        // Give the in-page navigation a moment to commit before reloading.
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.cdp_refresh_current(true).await
    }

    /// Set an error on the active tab without terminating the browser.
    fn record_navigation_error(&mut self, err: &anyhow::Error) {
        warn!("CDP navigation failed: {err:#}");
        let message = format!("{err:#}").replace('\n', " ");
        self.active_tab_mut().error = Some(message.chars().take(160).collect());
    }

    /// Fetch linked author stylesheets and attach them to their `<link>`
    /// nodes before layout. Most real sites put their entire design in these
    /// files; without them the cell-grid rendering is nearly unreadable.
    async fn load_stylesheets(&self, page: &mut Page) {
        // Off by default: Blitz 0.2 does not implement all of the selectors
        // modern sites use to hide menus, so applying their CSS can surface
        // hidden content and make pages less readable. Opt in when a site is
        // known to benefit (e.g. Wikipedia or Hacker News).
        if !std::env::var("MIL_LOAD_STYLESHEETS")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "on" | "true" | "yes"))
        {
            return;
        }
        const MAX_SHEETS: usize = 8;
        const MAX_BYTES: u64 = 2 * 1024 * 1024;
        let links: Vec<(usize, Url)> = JsDomMirror::from_document(&page.doc, page.url.as_str())
            .nodes
            .into_iter()
            .filter(|node| {
                node.tag.eq_ignore_ascii_case("link")
                    && node.attrs.get("rel").is_some_and(|rel| {
                        rel.split_whitespace()
                            .any(|value| value.eq_ignore_ascii_case("stylesheet"))
                    })
            })
            .filter_map(|node| {
                let href = node.attrs.get("href")?;
                let url = page.url.join(href).ok()?;
                Some((node.id, url))
            })
            .take(MAX_SHEETS)
            .collect();

        let mut total = 0_u64;
        for (node_id, url) in links {
            match self.client.get(&url).await {
                Ok(response) if response.status.is_success() => {
                    let len = response.body.len() as u64;
                    if total.saturating_add(len) > MAX_BYTES {
                        warn!("stylesheet budget exceeded, skipping {url}");
                        break;
                    }
                    total += len;
                    let css = String::from_utf8_lossy(&response.body);
                    if let Err(error) = page.add_stylesheet(node_id, &css) {
                        warn!("could not apply stylesheet {url}: {error:#}");
                    }
                }
                Ok(response) => warn!("stylesheet {url} returned {}", response.status),
                Err(error) => warn!("could not fetch stylesheet {url}: {error:#}"),
            }
        }
    }

    /// Execute inline scripts with the deliberately limited rquickjs façade.
    /// Failures are logged and never abort a page load.
    fn run_scripts(&self, html: &str, page: &mut Page, url: &Url) {
        let scripts: Vec<String> = JsDomMirror::from_document(&page.doc, url.as_str())
            .nodes
            .into_iter()
            .filter(|node| {
                node.tag.eq_ignore_ascii_case("script")
                    && !node.attrs.contains_key("src")
                    && !node.text.trim().is_empty()
            })
            .map(|node| node.text)
            .collect();
        if scripts.is_empty() {
            return;
        }

        let mut engine = JsEngine::with_timeout(self.config.javascript.timeout_ms);
        match engine.run(html, &scripts, url.as_str()) {
            Ok(report) => {
                for log in &report.logs {
                    debug!("js: {log}");
                }
                for error in &report.errors {
                    warn!("js error: {error}");
                }
                if let Err(error) = mil_js::apply_report(&mut page.doc, &report) {
                    warn!("could not apply JS mutations: {error:#}");
                }
                // Scripts may rewrite the title and the DOM; refresh the title
                // before re-layout so both paint and history use the new data.
                page.refresh_title();
                page.resolve(0.0);
            }
            Err(error) => warn!("js engine failed: {error:#}"),
        }
    }

    /// Update the tab's history stacks and return the URL to visit.
    fn history_target(&mut self, backwards: bool) -> Option<Url> {
        let tab = self.active_tab_mut();
        if backwards {
            if tab.back.is_empty() {
                return None;
            }
            let next = tab.back.pop().expect("checked non-empty");
            let current = tab.url.clone();
            tab.forward.push(current);
            Some(next)
        } else {
            if tab.forward.is_empty() {
                return None;
            }
            let next = tab.forward.pop().expect("checked non-empty");
            let current = tab.url.clone();
            tab.back.push(current);
            Some(next)
        }
    }

    /// History navigation through the active engine. Blitz keeps its original
    /// "clear then refetch" path; CDP asks the browser to go back/forward and
    /// reports failures in the status line without exiting.
    async fn history_move(&mut self, backwards: bool) -> Result<bool> {
        let Some(to) = self.history_target(backwards) else {
            return Ok(false);
        };
        debug!("history: {} -> {}", self.active_tab().url, to);
        if self.is_cdp() {
            let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let result = if backwards {
                self.cdp_engine()?.back(cols, rows).await
            } else {
                self.cdp_engine()?.forward(cols, rows).await
            };
            match result {
                Ok(page) => self.install_page(page.into(), false, rows),
                Err(err) => {
                    // Undo the bookkeeping so a retry targets the same entry.
                    let tab = self.active_tab_mut();
                    if backwards {
                        tab.forward.pop();
                        tab.back.push(to);
                    } else {
                        tab.back.pop();
                        tab.forward.push(to);
                    }
                    self.record_navigation_error(&err);
                }
            }
            return Ok(true);
        }
        let tab = self.active_tab_mut();
        tab.url = to;
        tab.page = None;
        tab.surface = None;
        tab.scroll_y = 0.0;
        tab.error = None;
        self.focused_link = 0;
        self.restore_active().await?;
        Ok(true)
    }

    fn save_session(&self) -> Result<()> {
        if !self.restore_session {
            return Ok(());
        }
        let session = Session {
            tabs: self.tabs.iter().map(Tab::snapshot).collect(),
            active: self.active.min(self.tabs.len().saturating_sub(1)),
        };
        session.save()
    }

    fn link_count(&self) -> usize {
        if self.is_cdp() {
            return self.active_tab().cdp_links.len();
        }
        self.active_tab()
            .page
            .as_ref()
            .map_or(0, |page| page.links().len())
    }

    /// Scroll to the first text run containing `query`.
    fn find_text(&mut self, query: &str) {
        let row = if self.is_cdp() {
            let Some(surface) = self.active_tab().surface.as_ref() else {
                return;
            };
            find_row_in_surface(surface, query)
        } else {
            let Some(page) = self.active_tab().page.as_ref() else {
                return;
            };
            find_row(page, query)
        };
        if let Some(row) = row {
            clamp_scroll(self.active_tab_mut(), row);
        }
    }

    /// Scroll the painted (full-document) surface and keep the CDP page's own
    /// scroll offset in sync with a pixel hint. `cdp.scroll` is advisory: the
    /// surface is a static snapshot, so scrolling never re-renders or reloads.
    async fn scroll_to(&mut self, target: f64) {
        let previous = self.active_tab().scroll_y;
        let tab = self.active_tab_mut();
        clamp_scroll(tab, target);
        let applied_rows = tab.scroll_y - previous;
        if self.is_cdp() && applied_rows != 0.0 {
            let px = applied_rows * f64::from(CELL_HEIGHT_PX);
            if let Some(cdp) = self.cdp.as_mut() {
                if let Err(err) = cdp.scroll(px).await {
                    warn!("could not send CDP scroll hint: {err:#}");
                }
            }
        }
    }

    async fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let mut url_input = self.active_tab().url.to_string();
        let mut editing_url = false;

        loop {
            let snapshot = self.tab_infos();
            let status = self.status_text();
            {
                // Split-borrow the fields so the immutable tab reference and
                // the mutable view can coexist for the duration of a frame.
                let App {
                    view, tabs, active, ..
                } = self;
                let tab = &tabs[*active];
                let state = ViewState {
                    tabs: &snapshot,
                    active: *active,
                    url_input: &url_input,
                    editing_url,
                    status: &status,
                    title: &tab.title,
                    // CDP pages deliberately have no Blitz `Page`, so the
                    // spinner must track the painted surface instead.
                    loading: tab.surface.is_none(),
                    surface: tab.surface.as_ref(),
                    scroll_y: tab.scroll_y,
                    scroll_max_y: tab.scroll_max_y,
                };
                view.render(terminal, &state)?;
            }

            if !event::poll(Duration::from_millis(30))? {
                continue;
            }
            let ev = event::read()?;
            // In CDP mode the page receives raw input so it can run its own
            // key handlers. Text entered while the omnibox owns input must not
            // leak into the page. Mouse clicks and wheels are excluded here:
            // the view turns them into `Action`s, which are dispatched to CDP
            // below as precise document-pixel clicks / scroll hints.
            //
            // Caveat: UI shortcuts (Ctrl-L, Ctrl-T, q, r, …) and find-prompt
            // typing also reach the page: the view's keybinding map and its
            // find state are private, so the app cannot tell which events the
            // TUI consumed. `CdpEngine::key_event` filters the worst cases
            // (Ctrl-L/C/Q, Esc), but this only loosely mirrors a real browser.
            if self.is_cdp() && !editing_url {
                let ui_mouse = matches!(
                    &ev,
                    Event::Mouse(mouse)
                        if matches!(
                            mouse.kind,
                            event::MouseEventKind::Down(event::MouseButton::Left)
                                | event::MouseEventKind::ScrollUp
                                | event::MouseEventKind::ScrollDown
                        )
                );
                if !ui_mouse {
                    if let Some(cdp) = self.cdp.as_mut() {
                        if let Err(err) = cdp.key_event(&ev).await {
                            warn!("could not forward event to CDP page: {err:#}");
                        }
                    }
                }
            }
            // The view owns all text state (omnibox, find prompt and paste).
            // Editing text here as well would swallow keybindings such as
            // Ctrl-C and Ctrl-W while the omnibox is focused.
            let action = self
                .view
                .handle_event(ev, editing_url, &self.config.keybindings);

            if editing_url && matches!(action, Some(Action::BlurUrl | Action::Navigate(_))) {
                editing_url = false;
            }

            match action {
                Some(Action::Quit) => return Ok(()),
                Some(Action::FocusUrl) => {
                    // An unfinished find prompt would otherwise consume the
                    // characters typed into the omnibox and its Enter key.
                    let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                    let _ = self.view.handle_event(esc, false, &self.config.keybindings);
                    // Start a fresh prompt so typing replaces the URL instead
                    // of appending to it (equivalent to select-all in a GUI
                    // browser's address bar).
                    url_input.clear();
                    editing_url = true;
                }
                Some(Action::BlurUrl) => {
                    editing_url = false;
                    url_input = self.active_tab().url.to_string();
                }
                Some(Action::Navigate(target)) => {
                    let url = resolve_input(&target, &self.config);
                    self.navigate(url, true).await?;
                    url_input = self.active_tab().url.to_string();
                }
                Some(Action::NewTab) => {
                    self.tabs.push(Tab::new(homepage_url(&self.config)));
                    self.active = self.tabs.len() - 1;
                    self.focused_link = 0;
                    editing_url = false;
                    url_input = self.active_tab().url.to_string();
                    self.restore_active().await?;
                }
                Some(Action::CloseTab) => {
                    if self.tabs.len() > 1 {
                        self.tabs.remove(self.active);
                        self.active = self.active.min(self.tabs.len() - 1);
                        self.focused_link = 0;
                        editing_url = false;
                        url_input = self.active_tab().url.to_string();
                        self.restore_active().await?;
                    } else {
                        return Ok(());
                    }
                }
                Some(Action::NextTab) => {
                    self.active = (self.active + 1) % self.tabs.len();
                    self.focused_link = 0;
                    editing_url = false;
                    url_input = self.active_tab().url.to_string();
                    self.restore_active().await?;
                }
                Some(Action::PrevTab) => {
                    self.active = self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1);
                    self.focused_link = 0;
                    editing_url = false;
                    url_input = self.active_tab().url.to_string();
                    self.restore_active().await?;
                }
                Some(Action::Reload) => {
                    if self.is_cdp() {
                        // Failures stay on the page as an error message.
                        if let Err(err) = self.cdp_refresh_current(false).await {
                            self.record_navigation_error(&err);
                        }
                    } else {
                        let url = self.active_tab().url.clone();
                        self.navigate(url, false).await?;
                    }
                    url_input = self.active_tab().url.to_string();
                }
                Some(Action::Back) => {
                    if self.history_move(true).await? {
                        editing_url = false;
                        url_input = self.active_tab().url.to_string();
                    }
                }
                Some(Action::Forward) => {
                    if self.history_move(false).await? {
                        editing_url = false;
                        url_input = self.active_tab().url.to_string();
                    }
                }
                Some(Action::Scroll(delta)) => {
                    let next = self.active_tab().scroll_y + f64::from(delta);
                    self.scroll_to(next).await;
                }
                Some(Action::ScrollToTop) => self.scroll_to(0.0).await,
                Some(Action::ScrollToBottom) => {
                    let max = self.active_tab().scroll_max_y;
                    self.scroll_to(max).await;
                }
                Some(Action::Click(col, row)) => {
                    self.follow_link_at(col, row).await?;
                }
                Some(Action::ActivateLink) => {
                    self.follow_focused_link().await?;
                }
                Some(Action::NextLink) => {
                    let count = self.link_count();
                    if count > 0 {
                        self.focused_link = (self.focused_link + 1) % count;
                    }
                }
                Some(Action::PrevLink) => {
                    let count = self.link_count();
                    if count > 0 {
                        self.focused_link = (self.focused_link + count - 1) % count;
                    }
                }
                Some(Action::Bookmark) => {
                    let tab = self.active_tab();
                    self.bookmarks.add(tab.title.clone(), tab.url.to_string());
                    if let Err(err) = self.bookmarks.save() {
                        warn!("could not save bookmark: {err:#}");
                    }
                }
                Some(Action::Find(query)) => self.find_text(&query),
                Some(Action::ToggleHelp) => {
                    // Help is rendered by the TUI status line in this build.
                }
                Some(Action::OpenNewTab(target)) => {
                    let url = resolve_input(&target, &self.config);
                    self.tabs.push(Tab::new(url.clone()));
                    self.active = self.tabs.len() - 1;
                    self.focused_link = 0;
                    editing_url = false;
                    self.navigate(url, false).await?;
                    url_input = self.active_tab().url.to_string();
                }
                None => {}
            }
        }
    }

    async fn follow_link_at(&mut self, col: u16, row: u16) -> Result<bool> {
        if self.is_cdp() {
            // `Action::Click` rows are document-relative: `TerminalView` has
            // already added its scroll offset, so adding `scroll_y` again
            // would double-count it here.
            let x = (f64::from(col) + 0.5) * f64::from(CELL_WIDTH_PX);
            let y = (f64::from(row) + 0.5) * f64::from(CELL_HEIGHT_PX);
            let link = cdp_link_at(&self.active_tab().cdp_links, x, y).cloned();
            match link {
                Some(link) => {
                    if let Err(err) = self.cdp_follow_link(link).await {
                        self.record_navigation_error(&err);
                    }
                }
                None => {
                    if let Err(err) = self.cdp_engine()?.click(x, y).await {
                        warn!("could not send CDP click: {err:#}");
                    }
                }
            }
            return Ok(true);
        }
        let links = {
            let Some(page) = self.active_tab().page.as_ref() else {
                return Ok(false);
            };
            page.links()
        };
        let Some(link) = link_at(&links, col, row) else {
            return Ok(false);
        };
        self.navigate(Url::parse(&link.url)?, true).await?;
        Ok(true)
    }

    async fn follow_focused_link(&mut self) -> Result<bool> {
        if self.is_cdp() {
            let len = self.active_tab().cdp_links.len();
            let index = self.focused_link.min(len.saturating_sub(1));
            let Some(link) = self.active_tab().cdp_links.get(index).cloned() else {
                return Ok(false);
            };
            // Use the centre of the link's document-pixel rectangle so the
            // real page receives a reliable hit on the anchor.
            if let Err(err) = self.cdp_follow_link(link).await {
                self.record_navigation_error(&err);
            }
            return Ok(true);
        }
        let links = {
            let Some(page) = self.active_tab().page.as_ref() else {
                return Ok(false);
            };
            page.links()
        };
        let index = self.focused_link.min(links.len().saturating_sub(1));
        let Some(link) = links.get(index) else {
            return Ok(false);
        };
        let url = Url::parse(&link.url)?;
        self.navigate(url, true).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::default()
    }

    #[test]
    fn resolves_absolute_url() {
        let url = resolve_input("https://example.org/a", &config());
        assert_eq!(url.as_str(), "https://example.org/a");
    }

    #[test]
    fn resolves_bare_host() {
        let url = resolve_input("example.org", &config());
        assert_eq!(url.as_str(), "https://example.org/");
    }

    #[test]
    fn resolves_localhost_without_tls() {
        assert_eq!(
            resolve_input("localhost:8000/path", &config()).as_str(),
            "http://localhost:8000/path"
        );
        assert_eq!(
            resolve_input("127.0.0.1:8080/path", &config()).as_str(),
            "http://127.0.0.1:8080/path"
        );
        assert_eq!(
            resolve_input("[::1]:8080/", &config()).as_str(),
            "http://[::1]:8080/"
        );
    }

    #[test]
    fn keeps_file_urls() {
        let url = resolve_input("file:///tmp/page.html", &config());
        assert_eq!(url.scheme(), "file");
        assert_eq!(url.path(), "/tmp/page.html");
    }

    #[test]
    fn resolves_search_query() {
        let url = resolve_input("hello world", &config());
        assert!(url.as_str().contains("q=hello+world"));
    }

    #[test]
    fn empty_input_uses_homepage() {
        assert_eq!(
            resolve_input("", &config()).as_str(),
            "https://example.com/"
        );
    }

    #[test]
    fn tab_snapshot_roundtrip() {
        let mut tab = Tab::new(Url::parse("https://a.example/").unwrap());
        tab.title = "A".into();
        tab.scroll_y = 3.5;
        tab.back.push(Url::parse("https://b.example/").unwrap());
        let restored = Tab::from_snapshot(&tab.snapshot()).unwrap();
        assert_eq!(restored.url, tab.url);
        assert_eq!(restored.title, "A");
        assert_eq!(restored.scroll_y, 3.5);
        assert_eq!(restored.back.len(), 1);
    }

    #[test]
    fn surface_rows_cover_long_documents() {
        let lines = (0..200)
            .map(|i| format!("<p>line {i}<br>"))
            .collect::<String>();
        let html = format!("<html><body>{lines}</body></html>");
        let viewport = Viewport::new(640, 320, 1.0, ColorScheme::Light);
        let mut page = Page::from_html(
            &html,
            &Url::parse("https://example.org/").unwrap(),
            viewport,
        )
        .expect("parse");
        page.resolve(0.0);
        assert!(
            surface_rows(&page, 20) > 20,
            "a long document must paint more rows than the viewport"
        );
        assert!(find_row(&page, "LINE 30").is_some_and(|row| row > 0.0));
        assert_eq!(find_row(&page, "no such phrase"), None);
    }

    #[test]
    fn clicks_map_to_document_cells_without_re_adding_scroll() {
        let link = LinkRegion {
            index: 0,
            url: "https://example.org/".to_owned(),
            x: 0.0,
            y: 16.0,
            width: 40.0,
            height: 16.0,
            node_id: 1,
        };
        // `Action::Click` rows are document-relative and already include the
        // view's scroll offset.
        let links = std::slice::from_ref(&link);
        assert!(link_at(links, 1, 1).is_some());
        assert!(link_at(links, 1, 0).is_none());
        assert!(link_at(links, 1, 2).is_none());
        assert!(link_at(links, 9, 1).is_none());
    }

    #[test]
    fn clamping_rejects_non_finite_scroll_bounds() {
        let mut tab = Tab::new(Url::parse("https://a.example/").unwrap());
        tab.scroll_max_y = f64::NAN;
        clamp_scroll(&mut tab, 12.0);
        assert_eq!(tab.scroll_y, 0.0);
        tab.scroll_max_y = 5.0;
        clamp_scroll(&mut tab, 12.0);
        assert_eq!(tab.scroll_y, 5.0);
        clamp_scroll(&mut tab, f64::NEG_INFINITY);
        assert_eq!(tab.scroll_y, 0.0);
    }

    #[test]
    fn engine_and_chrome_flags_parse() {
        let args = Args::try_parse_from([
            "mil",
            "--engine",
            "cdp",
            "--chrome",
            "/opt/chrome-headless-shell",
            "https://example.org",
        ])
        .expect("parse CDP flags");
        assert_eq!(args.engine, EngineMode::Cdp);
        assert_eq!(
            args.chrome,
            Some(PathBuf::from("/opt/chrome-headless-shell"))
        );
        assert_eq!(args.url.as_deref(), Some("https://example.org"));

        let default = Args::try_parse_from(["mil"]).expect("parse defaults");
        assert_eq!(default.engine, EngineMode::Blitz);
        assert!(default.chrome.is_none());
        assert!(Args::try_parse_from(["mil", "--engine", "firefox"]).is_err());
        assert_eq!(
            EngineMode::from_str("CDP", true).expect("case-insensitive"),
            EngineMode::Cdp
        );
    }

    #[test]
    fn cdp_links_hit_test_in_document_pixels() {
        let link = CdpLink {
            x: 8.0,
            y: 32.0,
            w: 40.0,
            h: 16.0,
            url: "https://example.org/".to_owned(),
        };
        let links = std::slice::from_ref(&link);
        // A click at cell (1, 2) maps to (12, 40) document pixels.
        assert!(cdp_link_at(links, 12.0, 40.0).is_some());
        assert!(cdp_link_at(links, 12.0, 16.0).is_none());
        assert!(cdp_link_at(links, 100.0, 40.0).is_none());
        assert_eq!(link.x + link.w / 2.0, 28.0);
    }

    #[test]
    fn cdp_surface_finds_text_by_row() {
        // Click rows from the view are already document-relative, so a find
        // result in the full-document surface can be used as `scroll_y` as-is.
        let mut surface = Surface::new(16, 4);
        for (col, ch) in "hello world".chars().enumerate() {
            surface.set(
                col as u16,
                2,
                mil_paint::Cell {
                    symbol: ch.to_string(),
                    ..mil_paint::Cell::blank()
                },
            );
        }
        assert_eq!(find_row_in_surface(&surface, "WORLD"), Some(2.0));
        assert_eq!(find_row_in_surface(&surface, "nope"), None);
    }
}
