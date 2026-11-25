//! Persistence for `mil`: TOML configuration, tab/session snapshots, and
//! bookmarks.
//!
//! This crate is intentionally independent of the rest of the workspace. It
//! owns the on-disk formats and the XDG path conventions used by the binary:
//!
//! - configuration is read from `MIL_CONFIG`, then
//!   `$XDG_CONFIG_HOME/mil/config.toml`, then `~/.config/mil/config.toml`;
//! - mutable state (session and bookmarks) lives under
//!   `$XDG_STATE_HOME/mil` or `~/.local/state/mil`.
//!
//! Every field is optional on disk: missing keys fall back to built-in
//! defaults, unknown keys are ignored, and invalid enumerated values (`theme`,
//! `images`) are reported with [`tracing::warn!`] and replaced by their
//! defaults. Writes are atomic: data is written to a uniquely named, private
//! file in the destination directory, fsynced, and renamed into place.
//! Empty session/bookmark files are treated as empty collections.
//!
//! ```
//! use mil_session::{Config, Session};
//!
//! let config = Config::default();
//! assert_eq!(config.homepage, "https://example.com");
//! assert!(Session::default().tabs.is_empty());
//! ```

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const DEFAULT_HOMEPAGE: &str = "https://example.com";
const DEFAULT_SEARCH_ENGINE: &str = "https://www.google.com/search?q={}";
const DEFAULT_PALETTE: &str = "auto";
const DEFAULT_IMAGE_POLICY: &str = "inline";
const DEFAULT_IMAGE_PROTOCOL: &str = "auto";
const DEFAULT_JS_TIMEOUT_MS: u64 = 1_000;
const DEFAULT_NETWORK_TIMEOUT_SECS: u64 = 30;

const PALETTES: &[&str] = &["auto", "dark", "light", "16", "256", "truecolor"];
const IMAGE_POLICIES: &[&str] = &["inline", "alt", "sampling", "omit"];
const IMAGE_PROTOCOLS: &[&str] = &["auto", "iterm2", "sixel", "kitty"];

/// User configuration, normally stored as TOML at [`Config::config_path`].
///
/// All fields have serde defaults, so a partial (or older) config file is
/// perfectly valid. Unknown keys are silently ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Page opened when no session is restored.
    #[serde(default = "default_homepage")]
    pub homepage: String,
    /// URL template for omnibox searches; `{}` is replaced by the query.
    #[serde(default = "default_search_engine")]
    pub search_engine: String,
    /// Optional `User-Agent` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Maximum layout width in columns; `None` follows the terminal width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u16>,
    /// Colour mode and palette overrides.
    #[serde(default = "default_theme")]
    pub theme: ThemeConfig,
    /// Key sequence to action-name mapping.
    #[serde(default = "default_keybindings")]
    pub keybindings: BTreeMap<String, String>,
    /// Image rendering settings.
    #[serde(default = "default_images")]
    pub images: ImageConfig,
    /// JavaScript settings.
    #[serde(default = "default_javascript")]
    pub javascript: JsConfig,
    /// Network settings.
    #[serde(default = "default_network")]
    pub network: NetworkConfig,
    /// Terminal UI chrome settings.
    #[serde(default = "default_ui")]
    pub ui: UiConfig,
}

/// Colour configuration. `palette` is one of `auto`, `dark`, `light`, `16`,
/// `256`, or `truecolor`; other values fall back to `auto` on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    #[serde(default = "default_palette")]
    pub palette: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// Image settings. `policy` is one of `inline`, `alt`, `sampling`, `omit`;
/// `protocol` is one of `auto`, `iterm2`, `sixel`, `kitty`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    #[serde(default = "default_image_policy")]
    pub policy: String,
    #[serde(default = "default_image_protocol")]
    pub protocol: String,
}

/// JavaScript execution settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_js_timeout_ms")]
    pub timeout_ms: u64,
}

/// Network settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    #[serde(default = "default_network_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
}

/// Terminal UI chrome settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    #[serde(default = "default_true")]
    pub show_tab_bar: bool,
    #[serde(default = "default_true")]
    pub show_status_bar: bool,
    #[serde(default)]
    pub smooth_scroll: bool,
}

fn default_homepage() -> String {
    DEFAULT_HOMEPAGE.to_string()
}

fn default_search_engine() -> String {
    DEFAULT_SEARCH_ENGINE.to_string()
}

fn default_palette() -> String {
    DEFAULT_PALETTE.to_string()
}

fn default_image_policy() -> String {
    DEFAULT_IMAGE_POLICY.to_string()
}

fn default_image_protocol() -> String {
    DEFAULT_IMAGE_PROTOCOL.to_string()
}

fn default_js_timeout_ms() -> u64 {
    DEFAULT_JS_TIMEOUT_MS
}

fn default_network_timeout_secs() -> u64 {
    DEFAULT_NETWORK_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

fn default_keybindings() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn default_theme() -> ThemeConfig {
    ThemeConfig::default()
}

fn default_images() -> ImageConfig {
    ImageConfig::default()
}

fn default_javascript() -> JsConfig {
    JsConfig::default()
}

fn default_network() -> NetworkConfig {
    NetworkConfig::default()
}

fn default_ui() -> UiConfig {
    UiConfig::default()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            homepage: default_homepage(),
            search_engine: default_search_engine(),
            user_agent: None,
            max_width: None,
            theme: ThemeConfig::default(),
            keybindings: default_keybindings(),
            images: ImageConfig::default(),
            javascript: JsConfig::default(),
            network: NetworkConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            palette: default_palette(),
            accent: None,
            link: None,
        }
    }
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            policy: default_image_policy(),
            protocol: default_image_protocol(),
        }
    }
}

impl Default for JsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: default_js_timeout_ms(),
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_network_timeout_secs(),
            proxy: None,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_tab_bar: true,
            show_status_bar: true,
            smooth_scroll: false,
        }
    }
}

impl Config {
    /// Load the configuration from [`Config::config_path`].
    ///
    /// A missing file is not an error: built-in defaults are returned.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::config_path())
    }

    /// Load and validate a TOML configuration from an explicit path.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "config not found; using defaults");
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reading config from {}", path.display()));
            }
        };

        let mut config: Self = toml::from_str(&raw)
            .with_context(|| format!("parsing config from {}", path.display()))?;
        config.validate();
        Ok(config)
    }

    /// Path used by [`Config::load`] and [`Config::save`].
    ///
    /// Precedence: `MIL_CONFIG`, then `XDG_CONFIG_HOME/mil/config.toml`, then
    /// `~/.config/mil/config.toml`.
    pub fn config_path() -> PathBuf {
        if let Some(path) = env_path("MIL_CONFIG") {
            return path;
        }
        // The XDG spec requires absolute paths; ignore relative overrides and
        // never let `dirs` reintroduce one.
        let base = xdg_env_path("XDG_CONFIG_HOME")
            .or_else(dirs::config_dir)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home_dir().join(".config"));
        base.join("mil").join("config.toml")
    }

    /// Directory for mutable state: `XDG_STATE_HOME/mil` or
    /// `~/.local/state/mil`.
    pub fn state_dir() -> PathBuf {
        let base = xdg_env_path("XDG_STATE_HOME")
            .unwrap_or_else(|| home_dir().join(".local").join("state"));
        base.join("mil")
    }

    /// Save this configuration as TOML to [`Config::config_path`].
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::config_path())
    }

    /// Save this configuration as TOML to an explicit path.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        let encoded = toml::to_string_pretty(self).context("serializing config")?;
        atomic_write(path, encoded.as_bytes())
    }

    /// Replace invalid enumerated values with their defaults.
    fn validate(&mut self) {
        validate_choice(
            &mut self.theme.palette,
            "theme.palette",
            PALETTES,
            DEFAULT_PALETTE,
        );
        validate_choice(
            &mut self.images.policy,
            "images.policy",
            IMAGE_POLICIES,
            DEFAULT_IMAGE_POLICY,
        );
        validate_choice(
            &mut self.images.protocol,
            "images.protocol",
            IMAGE_PROTOCOLS,
            DEFAULT_IMAGE_PROTOCOL,
        );
    }
}

/// A snapshot of one tab, including its back/forward history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabSnapshot {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    // Non-finite f64s serialize to `null` in JSON, which cannot be deserialized
    // back into f64. Sanitize them at both ends so a tab can never make the
    // whole session file unloadable.
    #[serde(
        default,
        serialize_with = "serialize_scroll",
        deserialize_with = "deserialize_scroll"
    )]
    pub scroll_x: f64,
    #[serde(
        default,
        serialize_with = "serialize_scroll",
        deserialize_with = "deserialize_scroll"
    )]
    pub scroll_y: f64,
    #[serde(default)]
    pub back: Vec<String>,
    #[serde(default)]
    pub forward: Vec<String>,
}

/// A saved browser session, normally stored as JSON at [`Session::path`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    #[serde(default)]
    pub tabs: Vec<TabSnapshot>,
    #[serde(default)]
    pub active: usize,
}

impl Session {
    /// Load the session from [`Session::path`], or return an empty session if
    /// the file does not exist yet.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::path())
    }

    /// Load a session snapshot from an explicit path.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "session not found; using empty session");
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading session from {}", path.display()));
            }
        };
        if raw.trim().is_empty() {
            tracing::debug!(path = %path.display(), "empty session file; using empty session");
            return Ok(Self::default());
        }
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing session from {}", path.display()))
    }

    /// Save this session as JSON to [`Session::path`].
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::path())
    }

    /// Save this session as JSON to an explicit path.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        let encoded = serde_json::to_string_pretty(self).context("serializing session")?;
        atomic_write(path, format!("{encoded}\n").as_bytes())
    }

    /// Location of the session snapshot inside [`Config::state_dir`].
    pub fn path() -> PathBuf {
        Config::state_dir().join("session.json")
    }
}

/// A collection of bookmarks, stored as JSON at [`Bookmarks::path`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Bookmarks {
    #[serde(default)]
    pub entries: Vec<Bookmark>,
}

/// One bookmark. `created_at` is a Unix timestamp in seconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bookmark {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created_at: u64,
}

impl Bookmarks {
    /// Load bookmarks from [`Bookmarks::path`], or return an empty collection
    /// if the file does not exist.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::path())
    }

    /// Load bookmarks from an explicit path.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "bookmarks not found; using empty set");
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading bookmarks from {}", path.display()));
            }
        };
        if raw.trim().is_empty() {
            tracing::debug!(path = %path.display(), "empty bookmarks file; using empty set");
            return Ok(Self::default());
        }
        serde_json::from_str(&raw)
            .with_context(|| format!("parsing bookmarks from {}", path.display()))
    }

    /// Save bookmarks as JSON to [`Bookmarks::path`].
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::path())
    }

    /// Save bookmarks as JSON to an explicit path.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        let encoded = serde_json::to_string_pretty(self).context("serializing bookmarks")?;
        atomic_write(path, format!("{encoded}\n").as_bytes())
    }

    /// Append a bookmark with no tags and the current Unix timestamp.
    pub fn add(&mut self, title: impl Into<String>, url: impl Into<String>) {
        self.entries.push(Bookmark {
            title: title.into(),
            url: url.into(),
            tags: Vec::new(),
            created_at: now_unix_secs(),
        });
    }

    /// Remove every bookmark whose URL matches exactly.
    pub fn remove(&mut self, url: &str) {
        self.entries.retain(|entry| entry.url != url);
    }

    /// Location of the bookmark file inside [`Config::state_dir`].
    pub fn path() -> PathBuf {
        Config::state_dir().join("bookmarks.json")
    }
}

/// Return the current Unix timestamp, or `0` if the system clock is before
/// the epoch (which should be impossible in practice, but must not panic).
fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Write a finite scroll position; map NaN/±∞ to zero so the JSON stays
/// round-trippable.
fn serialize_scroll<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_f64(if value.is_finite() { *value } else { 0.0 })
}

/// Accept a number or `null` (and map non-finite numbers to zero) so older
/// snapshots written before sanitization can still be loaded.
fn deserialize_scroll<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<f64>::deserialize(deserializer)?;
    Ok(value.filter(|value| value.is_finite()).unwrap_or(0.0))
}

fn validate_choice(value: &mut String, key: &'static str, allowed: &[&str], default: &'static str) {
    if allowed.contains(&value.as_str()) {
        return;
    }
    tracing::warn!(
        key = key,
        invalid = %value.as_str(),
        expected = ?allowed,
        default = default,
        "invalid configuration value; using default"
    );
    value.clear();
    value.push_str(default);
}

fn env_path(key: &str) -> Option<PathBuf> {
    absolute_or_relative_path(std::env::var_os(key), key, false)
}

/// Read an XDG base-directory override. Relative paths are invalid according
/// to the XDG spec and are treated as if the variable were unset.
fn xdg_env_path(key: &str) -> Option<PathBuf> {
    absolute_or_relative_path(std::env::var_os(key), key, true)
}

fn absolute_or_relative_path(
    value: Option<std::ffi::OsString>,
    key: &str,
    require_absolute: bool,
) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if require_absolute && !path.is_absolute() {
        tracing::warn!(key = key, value = %path.display(), "ignoring relative XDG path");
        return None;
    }
    Some(path)
}

fn home_dir() -> PathBuf {
    match dirs::home_dir() {
        Some(path) => path,
        None => PathBuf::from("."),
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write bytes to `path` atomically.
///
/// A hidden temporary file is created in the destination directory with
/// `create_new` (so an existing file can never be clobbered), written, fsynced,
/// closed, and renamed over the destination. A drop guard removes the
/// temporary file if anything fails before the rename.
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("path has no UTF-8 file name: {}", path.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    for attempt in 0..8u32 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{file_name}.{}.{nanos}.{counter}.{attempt}.tmp",
            std::process::id()
        ));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Config/session/bookmarks can contain proxy credentials and browsing
        // history; never widen a pre-existing file's permissions on rename.
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = match options.open(&temp_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("creating temporary file {}", temp_path.display()));
            }
        };
        let guard = TempGuard::new(temp_path.clone());

        file.write_all(bytes)
            .with_context(|| format!("writing temporary file {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("fsyncing temporary file {}", temp_path.display()))?;
        drop(file);

        fs::rename(&temp_path, path)
            .with_context(|| format!("renaming {} to {}", temp_path.display(), path.display()))?;
        guard.disarm();
        sync_parent_dir(parent);
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "could not create a unique temporary file for {}",
        path.display()
    ))
}

/// Removes an uncommitted temporary file when a write fails before the rename.
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Best-effort durability for the rename itself; a failure here is not fatal
/// because the destination file is already complete.
#[cfg(unix)]
fn sync_parent_dir(parent: &Path) {
    match File::open(parent) {
        Ok(dir) => {
            if let Err(err) = dir.sync_all() {
                tracing::debug!(path = %parent.display(), error = %err, "could not fsync directory");
            }
        }
        Err(err) => {
            tracing::debug!(path = %parent.display(), error = %err, "could not open directory for fsync");
        }
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn default_config_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let config = Config::default();
        config.save_to(&path)?;

        let loaded = Config::load_from(&path)?;
        assert_eq!(loaded.homepage, config.homepage);
        assert_eq!(loaded.search_engine, config.search_engine);
        assert_eq!(loaded.user_agent, config.user_agent);
        assert_eq!(loaded.max_width, config.max_width);
        assert_eq!(loaded.theme.palette, config.theme.palette);
        assert_eq!(loaded.theme.accent, config.theme.accent);
        assert_eq!(loaded.theme.link, config.theme.link);
        assert_eq!(loaded.keybindings, config.keybindings);
        assert_eq!(loaded.images.policy, config.images.policy);
        assert_eq!(loaded.images.protocol, config.images.protocol);
        assert_eq!(loaded.javascript.enabled, config.javascript.enabled);
        assert_eq!(loaded.javascript.timeout_ms, config.javascript.timeout_ms);
        assert_eq!(loaded.network.timeout_secs, config.network.timeout_secs);
        assert_eq!(loaded.network.proxy, config.network.proxy);
        assert_eq!(loaded.ui.show_tab_bar, config.ui.show_tab_bar);
        assert_eq!(loaded.ui.show_status_bar, config.ui.show_status_bar);
        assert_eq!(loaded.ui.smooth_scroll, config.ui.smooth_scroll);
        assert_eq!(
            toml::to_string_pretty(&loaded)?,
            toml::to_string_pretty(&config)?
        );
        Ok(())
    }

    #[test]
    fn partial_config_loads_with_defaults() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("partial.toml");
        fs::write(
            &path,
            r#"
homepage = "https://example.org"
unknown_key = 42

[theme]
palette = "dark"

[images]
policy = "alt"
"#,
        )?;

        let config = Config::load_from(&path)?;
        assert_eq!(config.homepage, "https://example.org");
        assert_eq!(config.search_engine, DEFAULT_SEARCH_ENGINE);
        assert_eq!(config.user_agent, None);
        assert_eq!(config.max_width, None);
        assert_eq!(config.theme.palette, "dark");
        assert_eq!(config.theme.accent, None);
        assert_eq!(config.images.policy, "alt");
        assert_eq!(config.images.protocol, DEFAULT_IMAGE_PROTOCOL);
        assert!(!config.javascript.enabled);
        assert_eq!(config.javascript.timeout_ms, DEFAULT_JS_TIMEOUT_MS);
        assert_eq!(config.network.timeout_secs, DEFAULT_NETWORK_TIMEOUT_SECS);
        assert!(config.ui.show_tab_bar);
        assert!(config.ui.show_status_bar);
        assert!(!config.ui.smooth_scroll);
        Ok(())
    }

    #[test]
    fn invalid_enums_fall_back_to_defaults() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("invalid.toml");
        fs::write(
            &path,
            r#"
[theme]
palette = "neon"

[images]
policy = "ascii"
protocol = "vga"
"#,
        )?;

        let config = Config::load_from(&path)?;
        assert_eq!(config.theme.palette, DEFAULT_PALETTE);
        assert_eq!(config.images.policy, DEFAULT_IMAGE_POLICY);
        assert_eq!(config.images.protocol, DEFAULT_IMAGE_PROTOCOL);
        Ok(())
    }

    #[test]
    fn missing_files_use_defaults() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(
            Config::load_from(&dir.path().join("config.toml"))?.homepage,
            DEFAULT_HOMEPAGE
        );
        assert!(
            Session::load_from(&dir.path().join("session.json"))?
                .tabs
                .is_empty()
        );
        assert!(
            Bookmarks::load_from(&dir.path().join("bookmarks.json"))?
                .entries
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn empty_config_file_uses_defaults() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "  \n")?;
        let config = Config::load_from(&path)?;
        assert_eq!(config.homepage, DEFAULT_HOMEPAGE);
        assert_eq!(config.search_engine, DEFAULT_SEARCH_ENGINE);
        assert_eq!(config.theme.palette, DEFAULT_PALETTE);
        Ok(())
    }

    #[test]
    fn atomic_save_leaves_no_temp_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nested").join("config.toml");
        let config = Config::default();

        config.save_to(&path)?;
        let entries =
            fs::read_dir(path.parent().expect("parent exists"))?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name().to_string_lossy(), "config.toml");

        // Overwriting an existing file must also be atomic and leave no debris.
        config.save_to(&path)?;
        let entries =
            fs::read_dir(path.parent().expect("parent exists"))?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(entries.len(), 1);
        assert_eq!(Config::load_from(&path)?.homepage, DEFAULT_HOMEPAGE);
        Ok(())
    }

    #[test]
    fn failed_write_cleans_up_temp_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("target");
        fs::create_dir(&target)?;

        // Renaming a file over a directory fails; the guard must remove the
        // temporary file so no partial state is left behind.
        assert!(Config::default().save_to(&target).is_err());
        let entries = fs::read_dir(dir.path())?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].path().is_dir());
        Ok(())
    }

    #[test]
    fn session_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.json");
        let session = Session {
            tabs: vec![
                TabSnapshot {
                    url: "https://example.com".to_string(),
                    title: "Example".to_string(),
                    scroll_x: 0.0,
                    scroll_y: 12.5,
                    back: vec!["https://previous.example".to_string()],
                    forward: Vec::new(),
                },
                TabSnapshot {
                    url: "about:blank".to_string(),
                    title: String::new(),
                    scroll_x: 3.0,
                    scroll_y: 0.0,
                    back: Vec::new(),
                    forward: vec!["https://next.example".to_string()],
                },
            ],
            active: 1,
        };

        session.save_to(&path)?;
        let loaded = Session::load_from(&path)?;
        assert_eq!(loaded.tabs, session.tabs);
        assert_eq!(loaded.active, session.active);
        Ok(())
    }

    #[test]
    fn bookmarks_add_remove_and_roundtrip() -> anyhow::Result<()> {
        let mut bookmarks = Bookmarks::default();
        bookmarks.add("Example", "https://example.com");
        bookmarks.add("Docs", "https://docs.example.com");

        assert_eq!(bookmarks.entries.len(), 2);
        assert_eq!(bookmarks.entries[0].title, "Example");
        assert_eq!(bookmarks.entries[0].url, "https://example.com");
        assert!(bookmarks.entries[0].tags.is_empty());
        assert!(bookmarks.entries[0].created_at > 0);

        bookmarks.remove("https://example.com");
        assert_eq!(bookmarks.entries.len(), 1);
        assert_eq!(bookmarks.entries[0].url, "https://docs.example.com");

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bookmarks.json");
        bookmarks.save_to(&path)?;
        let loaded = Bookmarks::load_from(&path)?;
        assert_eq!(loaded.entries, bookmarks.entries);
        Ok(())
    }

    #[test]
    fn empty_session_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.json");
        let session = Session {
            tabs: Vec::new(),
            active: 0,
        };
        session.save_to(&path)?;
        let loaded = Session::load_from(&path)?;
        assert!(loaded.tabs.is_empty());
        assert_eq!(loaded.active, 0);

        // A hand-written snapshot without `active` is valid too.
        fs::write(&path, r#"{"tabs":[]}"#)?;
        let loaded = Session::load_from(&path)?;
        assert!(loaded.tabs.is_empty());
        assert_eq!(loaded.active, 0);
        Ok(())
    }

    #[test]
    fn non_finite_scroll_positions_never_break_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.json");
        let session = Session {
            tabs: vec![TabSnapshot {
                url: "https://example.com".to_string(),
                title: String::new(),
                scroll_x: f64::NAN,
                scroll_y: f64::NEG_INFINITY,
                back: Vec::new(),
                forward: Vec::new(),
            }],
            active: 0,
        };
        session.save_to(&path)?;

        let raw = fs::read_to_string(&path)?;
        assert!(
            !raw.contains("null"),
            "non-finite scroll positions must not be written as null: {raw}"
        );
        let loaded = Session::load_from(&path)?;
        assert_eq!(loaded.tabs[0].scroll_x, 0.0);
        assert_eq!(loaded.tabs[0].scroll_y, 0.0);

        // Snapshots written before sanitization (or by hand) may contain null.
        fs::write(
            &path,
            r#"{"tabs":[{"scroll_x":null,"scroll_y":null}],"active":0}"#,
        )?;
        let loaded = Session::load_from(&path)?;
        assert_eq!(loaded.tabs[0].scroll_x, 0.0);
        assert_eq!(loaded.tabs[0].scroll_y, 0.0);
        Ok(())
    }

    #[test]
    fn empty_state_files_use_defaults() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let session_path = dir.path().join("session.json");
        let bookmarks_path = dir.path().join("bookmarks.json");
        fs::write(&session_path, "  \n")?;
        fs::write(&bookmarks_path, "")?;
        assert!(Session::load_from(&session_path)?.tabs.is_empty());
        assert!(Bookmarks::load_from(&bookmarks_path)?.entries.is_empty());
        Ok(())
    }

    #[test]
    fn malformed_files_report_errors_with_context() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("config.toml");
        let session_path = dir.path().join("session.json");
        fs::write(&config_path, "homepage = 42")?;
        fs::write(&session_path, "not json")?;

        let config_error = Config::load_from(&config_path).unwrap_err().to_string();
        assert!(config_error.contains("parsing config"), "{config_error}");
        let session_error = Session::load_from(&session_path).unwrap_err().to_string();
        assert!(session_error.contains("parsing session"), "{session_error}");
        Ok(())
    }

    #[test]
    fn path_overrides_follow_xdg_rules() {
        assert_eq!(
            absolute_or_relative_path(Some("/tmp/mil-config".into()), "XDG_CONFIG_HOME", true),
            Some(PathBuf::from("/tmp/mil-config"))
        );
        assert_eq!(
            absolute_or_relative_path(Some("relative/config".into()), "XDG_CONFIG_HOME", true),
            None
        );
        // MIL_CONFIG is an explicit file path, so relative values are allowed.
        assert_eq!(
            absolute_or_relative_path(Some("mil.toml".into()), "MIL_CONFIG", false),
            Some(PathBuf::from("mil.toml"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_files_private() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        Config::default().save_to(&path)?;
        assert_eq!(fs::metadata(&path)?.mode() & 0o777, 0o600);

        // Replacing a world-readable file must not preserve its loose mode.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        Config::default().save_to(&path)?;
        assert_eq!(fs::metadata(&path)?.mode() & 0o777, 0o600);
        Ok(())
    }
}
