//! Async HTTP layer for the `mil` terminal browser.
//!
//! # Design
//!
//! [`Client`] wraps a single [`reqwest::Client`] rebuilt per [`NetConfig`].
//! Requests are made over rustls (no OpenSSL), transparently decompressed for
//! gzip and brotli, and follow up to ten redirects. The final URL and all
//! response headers are preserved in [`Response`].
//!
//! Cookies are handled **manually** with `cookie_store`: `Set-Cookie` headers
//! are parsed into a shared jar and matching `Cookie` headers are attached to
//! each request. Reqwest's own `cookies` feature is intentionally left off so
//! there is only one jar. Note that `cookie_store` 0.22 uses `cookie` 0.18
//! internally (not the workspace's `cookie` 0.22); this crate never names the
//! `cookie` crate directly and instead uses `CookieStore::parse` and
//! `cookie_store::serde::json`, so no incompatible cookie types enter the
//! dependency graph.
//!
//! A small cache sits in front of the network: [`cache_key`] hashes a URL to a
//! filename, an in-memory insertion-ordered map keyed by the full request URL
//! acts as an approximate LRU bounded by [`NetConfig::max_cache_bytes`], and an
//! optional on-disk cache stores a raw body plus JSON metadata under
//! [`NetConfig::cache_dir`]. Only successful GET responses are cached, and
//! `no-store`/`no-cache`/`private` directives are honoured. Responses to
//! requests that carried a `Cookie` header, and responses that vary on request
//! headers, are never cached.
//!
//! # Limitations
//!
//! - This is not a full RFC 9111 cache: no freshness lifetimes, conditional
//!   requests or stale-while-revalidate logic. `Vary` is handled conservatively
//!   by refusing to store entries that vary on anything other than the already
//!   normalised `Accept-Encoding`.
//! - `Set-Cookie` headers on *intermediate* redirect responses are not visible
//!   to reqwest's redirect policy, so only cookies on the final response are
//!   captured.
//! - Disk-cache I/O is synchronous but limited to two small files per entry.
//! - Cookie save/load uses `cookie_store`'s JSON representation, which is
//!   specific to that crate and is not Netscape/Mozilla cookie-file format.
//! - The jar has no public-suffix list installed, so only single-label public
//!   suffixes (e.g. `Domain=com`) are rejected explicitly; multi-label
//!   suffixes such as `co.uk` are not covered.
//! - FNV-1a cache keys are not cryptographic. A collision merely causes a disk
//!   miss (the stored metadata records the original request URL, which is
//!   checked before use), or replaces another entry on disk.

#![forbid(unsafe_code)]

use std::{
    fmt::Write as _,
    fs::File,
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use parking_lot::Mutex;
use reqwest::{
    RequestBuilder,
    header::{
        ACCEPT_LANGUAGE, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, SET_COOKIE, USER_AGENT,
    },
};
use thiserror::Error;
use url::Url;

mod cache;
pub use cache::cache_key;
use cache::{MemoryCache, disk_read, disk_write, is_cacheable};

/// Configuration for a [`Client`].
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Value sent in the `User-Agent` header.
    pub user_agent: String,
    /// Total deadline for a request, from connect through body completion.
    pub timeout: Duration,
    /// Optional HTTP(S) proxy URL.
    pub proxy: Option<String>,
    /// Optional directory for the on-disk response cache.
    pub cache_dir: Option<PathBuf>,
    /// Byte budget for the in-memory cache; `0` disables caching entirely.
    pub max_cache_bytes: u64,
    /// Value sent in the `Accept-Language` header.
    pub accept_language: String,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            user_agent: concat!(
                "Mozilla/5.0 (compatible; mil/",
                env!("CARGO_PKG_VERSION"),
                "; +https://github.com/SaibaWaipu/mil)"
            )
            .to_owned(),
            timeout: Duration::from_secs(30),
            proxy: None,
            cache_dir: None,
            max_cache_bytes: 32 * 1024 * 1024,
            accept_language: "en-US,en;q=0.9".to_owned(),
        }
    }
}

/// Errors originating in the network layer.
///
/// These are wrapped in [`anyhow::Error`] by the public API; callers can
/// downcast to them when they need structured handling.
#[derive(Debug, Error)]
pub enum NetError {
    /// Sending the request or receiving response headers failed.
    #[error("request to {url} failed: {source}")]
    Request {
        /// URL that was requested.
        url: String,
        /// Underlying reqwest error.
        #[source]
        source: reqwest::Error,
    },
    /// The response body could not be read in full.
    #[error("reading response body from {url} failed: {source}")]
    Body {
        /// URL that was requested.
        url: String,
        /// Underlying reqwest error.
        #[source]
        source: reqwest::Error,
    },
}

/// A fetched HTTP response.
#[derive(Debug, Clone)]
pub struct Response {
    /// Final URL after redirects.
    pub url: Url,
    /// HTTP status code.
    pub status: reqwest::StatusCode,
    /// Fully decompressed response body.
    pub body: Bytes,
    /// `Content-Type` header value, if present and valid UTF-8.
    pub content_type: Option<String>,
    /// All response headers, including `Set-Cookie`.
    pub headers: HeaderMap,
}

/// An async HTTP client with manual cookies and a small response cache.
///
/// The client is `Send + Sync` and can be shared (e.g. behind an `Arc`) across
/// tasks; the cookie jar and cache use short-lived `parking_lot` locks.
pub struct Client {
    http: reqwest::Client,
    jar: Mutex<cookie_store::CookieStore>,
    cache: Mutex<MemoryCache>,
    disk_dir: Option<PathBuf>,
    cache_enabled: bool,
}

impl Client {
    /// Build a client from `config`.
    ///
    /// # Errors
    ///
    /// Returns an error if the user agent or accept-language is not a valid
    /// HTTP header value, if the proxy URL is invalid, or if reqwest cannot
    /// initialise its TLS backend.
    pub fn new(config: NetConfig) -> Result<Self> {
        let mut default_headers = HeaderMap::new();
        let user_agent =
            HeaderValue::from_str(&config.user_agent).context("invalid User-Agent header value")?;
        let accept_language = HeaderValue::from_str(&config.accept_language)
            .context("invalid Accept-Language header value")?;
        default_headers.insert(USER_AGENT, user_agent);
        default_headers.insert(ACCEPT_LANGUAGE, accept_language);

        let mut builder = reqwest::Client::builder()
            .default_headers(default_headers)
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .gzip(true)
            .brotli(true);
        if let Some(proxy) = config.proxy.as_deref() {
            let proxy = reqwest::Proxy::all(proxy).context("invalid proxy URL")?;
            builder = builder.proxy(proxy);
        }
        let http = builder.build().context("build reqwest client")?;

        Ok(Self {
            http,
            jar: Mutex::new(cookie_store::CookieStore::new()),
            cache: Mutex::new(MemoryCache::new(config.max_cache_bytes)),
            disk_dir: config.cache_dir,
            cache_enabled: config.max_cache_bytes > 0,
        })
    }

    /// Fetch `url` with GET, consulting the cache first.
    ///
    /// # Errors
    ///
    /// Returns an error on transport failures or if the body cannot be read.
    /// HTTP error statuses are returned as successful [`Response`] values.
    pub async fn get(&self, url: &Url) -> Result<Response> {
        if self.cache_enabled {
            // The in-memory map is keyed by the full URL, not the hash: a hash
            // collision must not turn into a cache hit for the wrong document.
            let hit = self.cache.lock().get(url.as_str());
            if let Some(response) = hit {
                return Ok(response);
            }
            let key = cache_key(url);
            if let Some(response) = disk_read(self.disk_dir.as_deref(), &key, url) {
                self.cache
                    .lock()
                    .put(url.as_str().to_owned(), response.clone());
                return Ok(response);
            }
        }
        self.send_request(self.http.get(url.clone()), url, true)
            .await
    }

    /// Submit `fields` as `application/x-www-form-urlencoded` via POST.
    ///
    /// # Errors
    ///
    /// Returns an error on transport failures or if the body cannot be read.
    pub async fn post_form(&self, url: &Url, fields: &[(String, String)]) -> Result<Response> {
        let body = encode_form(fields);
        let builder = self
            .http
            .post(url.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
        self.send_request(builder, url, false).await
    }

    /// Parse `Set-Cookie` headers from a response into the shared jar.
    ///
    /// Malformed cookies are skipped with a debug log rather than failing the
    /// request.
    pub fn set_cookie_from_response(&self, url: &Url, headers: &HeaderMap) {
        let mut jar = self.jar.lock();
        for value in headers.get_all(SET_COOKIE).iter() {
            let Ok(cookie) = value.to_str() else {
                continue;
            };
            // `cookie_store` only rejects public-suffix domains when a suffix
            // list is installed, and `CookieStore::new()` installs none. Block
            // the common `Domain=com` supercookie case explicitly.
            if has_single_label_domain(cookie) {
                tracing::debug!(url = %url, "ignoring Set-Cookie with public-suffix Domain");
                continue;
            }
            if let Err(err) = jar.parse(cookie, url) {
                tracing::debug!(%err, url = %url, "ignoring invalid Set-Cookie header");
            }
        }
    }

    /// Build the value for a `Cookie` request header, if any cookies match.
    ///
    /// Cookies are separated by `"; "` in the usual `name=value` form.
    pub fn cookie_header(&self, url: &Url) -> Option<String> {
        let jar = self.jar.lock();
        let mut header = String::new();
        for (name, value) in jar.get_request_values(url) {
            if !header.is_empty() {
                header.push_str("; ");
            }
            let _ = write!(header, "{name}={value}");
        }
        if header.is_empty() {
            None
        } else {
            Some(header)
        }
    }

    /// Persist unexpired, persistent cookies as JSON at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be created or serialisation fails.
    pub fn save_cookies(&self, path: &Path) -> Result<()> {
        let file =
            File::create(path).with_context(|| format!("create cookie file {}", path.display()))?;
        let mut writer = BufWriter::new(file);
        // `cookie_store`'s serde helpers return `Box<dyn Error + Send + Sync>`,
        // which `anyhow::Context` cannot wrap directly; convert it first.
        cookie_store::serde::json::save(&self.jar.lock(), &mut writer)
            .map_err(|err| std::io::Error::other(err.to_string()))
            .with_context(|| format!("write cookies to {}", path.display()))?;
        writer
            .flush()
            .with_context(|| format!("flush cookie file {}", path.display()))?;
        Ok(())
    }

    /// Replace the jar with cookies loaded from the JSON file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or parsed.
    pub fn load_cookies(&self, path: &Path) -> Result<()> {
        let file =
            File::open(path).with_context(|| format!("open cookie file {}", path.display()))?;
        let loaded = cookie_store::serde::json::load(std::io::BufReader::new(file))
            .map_err(|err| std::io::Error::other(err.to_string()))
            .with_context(|| format!("read cookies from {}", path.display()))?;
        *self.jar.lock() = loaded;
        Ok(())
    }

    /// Common send/decode/cookie/cache path for GET and POST.
    async fn send_request(
        &self,
        builder: RequestBuilder,
        request_url: &Url,
        cacheable_method: bool,
    ) -> Result<Response> {
        let cookie = self.cookie_header(request_url);
        let builder = match &cookie {
            Some(value) => builder.header(COOKIE, value.as_str()),
            None => builder,
        };
        let raw = builder.send().await.map_err(|source| NetError::Request {
            url: request_url.to_string(),
            source,
        })?;

        let status = raw.status();
        let headers = raw.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let final_url = raw.url().clone();
        let body = raw.bytes().await.map_err(|source| NetError::Body {
            url: request_url.to_string(),
            source,
        })?;

        let response = Response {
            url: final_url,
            status,
            body,
            content_type,
            headers,
        };
        self.set_cookie_from_response(&response.url, &response.headers);

        // A response to an authenticated request may be private even when the
        // server forgets to send `Vary: Cookie`; never share it via the cache.
        if self.cache_enabled && cacheable_method && cookie.is_none() && is_cacheable(&response) {
            let key = cache_key(request_url);
            self.cache
                .lock()
                .put(request_url.as_str().to_owned(), response.clone());
            disk_write(self.disk_dir.as_deref(), &key, request_url, &response);
        }
        Ok(response)
    }
}

/// Encode form fields with `application/x-www-form-urlencoded` rules.
fn encode_form(fields: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

/// Whether a `Set-Cookie` header explicitly targets a single-label domain.
///
/// A single label is a public suffix (`com`, `net`, ...); accepting such a
/// `Domain` attribute would let one host install a cookie sent to every other
/// host under that suffix. `cookie_store` performs this check only when a
/// public-suffix list is supplied, which `CookieStore::new()` does not do.
fn has_single_label_domain(cookie: &str) -> bool {
    cookie.split(';').skip(1).any(|attribute| {
        let Some((name, value)) = attribute.split_once('=') else {
            return false;
        };
        if !name.trim().eq_ignore_ascii_case("domain") {
            return false;
        }
        let domain = value.trim().trim_start_matches('.');
        !domain.is_empty() && !domain.contains('.')
    })
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use reqwest::{
        StatusCode,
        header::{HeaderMap, HeaderValue, SET_COOKIE},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::{Client, NetConfig, Response, encode_form};

    /// Minimal single-connection-per-request HTTP/1.1 test server.
    async fn serve(raw_response: String) -> (url::Url, Arc<AtomicUsize>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local address");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&chunk[..read]),
                    }
                }
                if stream.write_all(raw_response.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let url = url::Url::parse(&format!("http://{address}/index.html")).expect("test URL");
        (url, hits, handle)
    }

    fn test_client() -> Client {
        Client::new(NetConfig {
            timeout: Duration::from_secs(5),
            ..NetConfig::default()
        })
        .expect("client")
    }

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cookie_header_is_formatted_and_path_scoped() {
        let client = test_client();
        let url = url::Url::parse("https://example.com/app/page").expect("URL");
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("session=abc123; Path=/app; HttpOnly"),
        );
        headers.append(SET_COOKIE, HeaderValue::from_static("theme=dark; Path=/"));
        client.set_cookie_from_response(&url, &headers);

        let header = client.cookie_header(&url).expect("cookie header");
        assert!(header.contains("session=abc123"), "{header}");
        assert!(header.contains("theme=dark"), "{header}");

        let outside = url::Url::parse("https://example.com/other").expect("URL");
        assert!(
            !client
                .cookie_header(&outside)
                .unwrap_or_default()
                .contains("session=abc123")
        );
    }

    #[test]
    fn form_encoding_follows_url_rules() {
        let fields = vec![
            ("q".to_owned(), "hello world & more".to_owned()),
            ("lang".to_owned(), "en-US".to_owned()),
        ];
        assert_eq!(encode_form(&fields), "q=hello+world+%26+more&lang=en-US");
    }

    #[tokio::test]
    async fn get_builds_response_from_local_server() {
        let body = "<html>hello</html>";
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, hits, handle) = serve(raw).await;
        let response: Response = test_client().get(&url).await.expect("fetch");

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(&response.body[..], body.as_bytes());
        assert_eq!(
            response.content_type.as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(response.url, url);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn second_get_is_served_from_cache() {
        let body = "cached body";
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: max-age=60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, hits, handle) = serve(raw).await;
        let client = test_client();

        let first = client.get(&url).await.expect("first fetch");
        let second = client.get(&url).await.expect("second fetch");
        assert_eq!(first.body, second.body);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn non_success_status_is_not_cached() {
        let body = "server error";
        let raw = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, hits, handle) = serve(raw).await;
        let client = test_client();

        let first = client.get(&url).await.expect("first fetch");
        let second = client.get(&url).await.expect("second fetch");
        assert_eq!(first.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(second.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    #[tokio::test]
    async fn disk_cache_survives_a_new_client() {
        let body = "on disk";
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, hits, handle) = serve(raw).await;
        let directory = TempDir(
            std::env::temp_dir().join(format!("mil-net-disk-cache-{}", std::process::id())),
        );
        std::fs::create_dir_all(&directory.0).expect("temp dir");
        let config = NetConfig {
            cache_dir: Some(directory.0.clone()),
            timeout: Duration::from_secs(5),
            ..NetConfig::default()
        };

        let first_client = Client::new(config.clone()).expect("first client");
        let first = first_client.get(&url).await.expect("first fetch");
        assert_eq!(&first.body[..], body.as_bytes());

        let second_client = Client::new(config).expect("second client");
        let second = second_client.get(&url).await.expect("cached fetch");
        assert_eq!(second.body, first.body);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        handle.abort();
    }

    #[test]
    fn cookies_respect_secure_and_public_suffix_scoping() {
        let client = test_client();
        let https_url = url::Url::parse("https://example.com/login").expect("URL");
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("secure_only=yes; Path=/; Secure"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("tracker=yes; Domain=com; Path=/"),
        );
        client.set_cookie_from_response(&https_url, &headers);

        assert_eq!(
            client.cookie_header(&https_url).as_deref(),
            Some("secure_only=yes")
        );
        let http_url = url::Url::parse("http://example.com/login").expect("URL");
        assert!(client.cookie_header(&http_url).is_none());
        // A cookie scoped to `com` must not leak to unrelated hosts under it.
        let other = url::Url::parse("https://unrelated.com/").expect("URL");
        assert!(client.cookie_header(&other).is_none());
    }

    #[test]
    fn client_get_future_is_send() {
        // A parking_lot guard held across `.await` would make this future
        // !Send and fail to compile.
        fn assert_send<T: Send>(_: T) {}
        let client = test_client();
        let url = url::Url::parse("https://example.invalid/").expect("URL");
        assert_send(client.get(&url));
    }

    #[tokio::test]
    async fn cookie_bearing_requests_are_not_cached() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hit = counter.fetch_add(1, Ordering::SeqCst);
                let body = if hit == 0 { "first" } else { "second" };
                let raw = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(raw.as_bytes()).await;
            }
        });
        let url = url::Url::parse(&format!("http://{address}/page")).expect("URL");
        let client = test_client();
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_static("sid=abc; Path=/"));
        client.set_cookie_from_response(&url, &headers);
        assert!(client.cookie_header(&url).is_some());

        let first = client.get(&url).await.expect("first fetch");
        let second = client.get(&url).await.expect("second fetch");
        assert_eq!(&first.body[..], b"first");
        assert_eq!(&second.body[..], b"second");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    #[test]
    fn cookies_survive_save_and_load() {
        let directory =
            TempDir(std::env::temp_dir().join(format!("mil-net-cookies-{}", std::process::id())));
        std::fs::create_dir_all(&directory.0).expect("temp dir");
        let path = directory.0.join("cookies.json");
        let url = url::Url::parse("https://example.com/").expect("URL");
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("persistent=yes; Path=/; Max-Age=3600"),
        );

        let original = test_client();
        original.set_cookie_from_response(&url, &headers);
        original.save_cookies(&path).expect("save cookies");

        let loaded = test_client();
        loaded.load_cookies(&path).expect("load cookies");
        assert!(
            loaded
                .cookie_header(&url)
                .expect("cookie header")
                .contains("persistent=yes")
        );
    }
}
