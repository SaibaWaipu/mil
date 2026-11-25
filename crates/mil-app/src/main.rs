//! `mil` — a fast, minimal terminal web browser.
//!
//! The event loop is intentionally single-threaded: Blitz documents are
//! `!Send`, and a browser tab is naturally a single-threaded state machine.

use std::{io, path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow};
use blitz_traits::shell::{ColorScheme, Viewport};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
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

    let mut app = App {
        config,
        client,
        view,
        tabs: Vec::new(),
        active: 0,
        bookmarks,
        restore_session: !args.no_session,
        focused_link: 0,
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

    // Cleanup must run on every exit path. In particular, a failure to disable
    // mouse capture must not skip leaving raw mode / the alternate screen.
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
    let filter = format!("mil={level},mil_tui={level},mil_net={level},mil_dom={level}");
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
        "https://www.google.com/search?q={}".to_owned()
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

struct Tab {
    url: Url,
    title: String,
    back: Vec<Url>,
    forward: Vec<Url>,
    page: Option<Page>,
    surface: Option<Surface>,
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
}

impl App {
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
        if self.config.javascript.enabled {
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
        page.resolve(0.0);
        if self.config.javascript.enabled {
            self.run_scripts(&html, &mut page, &final_url);
        }
        let title = page.title().to_owned();

        let config = PaintConfig::default();
        let surface = render_page(&page, &config, cols, surface_rows(&page, rows))
            .with_context(|| format!("painting {}", final_url))?;
        // The tab bar is only drawn with more than one tab; the URL and status
        // rows are always present.
        let chrome = if self.tabs.len() > 1 { 3 } else { 2 };
        let visible_rows = rows.saturating_sub(chrome).max(1);
        let scroll_max_y = surface.height.saturating_sub(visible_rows) as f64;

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
        tab.page = Some(page);
        tab.surface = Some(surface);
        tab.scroll_y = 0.0;
        tab.scroll_max_y = scroll_max_y.max(0.0);
        tab.error = None;
        self.focused_link = 0;
        Ok(())
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

    fn navigate_history(&mut self, backwards: bool) -> Result<bool> {
        let tab = self.active_tab_mut();
        let to = if backwards {
            if tab.back.is_empty() {
                return Ok(false);
            }
            let next = tab.back.pop().expect("checked non-empty");
            let current = tab.url.clone();
            tab.forward.push(current);
            next
        } else {
            if tab.forward.is_empty() {
                return Ok(false);
            }
            let next = tab.forward.pop().expect("checked non-empty");
            let current = tab.url.clone();
            tab.back.push(current);
            next
        };
        debug!("history: {} -> {}", self.active_tab().url, to);
        let tab = self.active_tab_mut();
        tab.url = to;
        tab.page = None;
        tab.surface = None;
        tab.scroll_y = 0.0;
        tab.error = None;
        self.focused_link = 0;
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
        self.active_tab()
            .page
            .as_ref()
            .map_or(0, |page| page.links().len())
    }

    /// Scroll to the first text run containing `query`.
    fn find_text(&mut self, query: &str) {
        let row = {
            let Some(page) = self.active_tab().page.as_ref() else {
                return;
            };
            find_row(page, query)
        };
        if let Some(row) = row {
            clamp_scroll(self.active_tab_mut(), row);
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
                    loading: tab.page.is_none(),
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
                    url_input = self.active_tab().url.to_string();
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
                    let url = self.active_tab().url.clone();
                    self.navigate(url, false).await?;
                    url_input = self.active_tab().url.to_string();
                }
                Some(Action::Back) => {
                    if self.navigate_history(true)? {
                        self.restore_active().await?;
                        editing_url = false;
                        url_input = self.active_tab().url.to_string();
                    }
                }
                Some(Action::Forward) => {
                    if self.navigate_history(false)? {
                        self.restore_active().await?;
                        editing_url = false;
                        url_input = self.active_tab().url.to_string();
                    }
                }
                Some(Action::Scroll(delta)) => {
                    let tab = self.active_tab_mut();
                    let next = tab.scroll_y + f64::from(delta);
                    clamp_scroll(tab, next);
                }
                Some(Action::ScrollToTop) => clamp_scroll(self.active_tab_mut(), 0.0),
                Some(Action::ScrollToBottom) => {
                    let max = self.active_tab().scroll_max_y;
                    clamp_scroll(self.active_tab_mut(), max);
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
}
