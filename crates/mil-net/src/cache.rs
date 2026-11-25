//! Cache internals: a bounded in-memory LRU and an optional tiny on-disk cache.
//!
//! This is deliberately *not* a complete RFC 9111 cache: there is no freshness
//! lifetime, revalidation or conditional-request support, and `Vary` is
//! handled by refusing to cache rather than by varying the key. It is a
//! best-effort "don't download the same document twice this session" cache.

use std::path::Path;

use bytes::Bytes;
use indexmap::IndexMap;
use reqwest::StatusCode;
use reqwest::header::{
    CACHE_CONTROL, HeaderMap, HeaderName, HeaderValue, PRAGMA, SET_COOKIE, VARY,
};
use serde::{Deserialize, Serialize};
use tracing::warn;
use url::Url;

use crate::Response;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Stable, dependency-free hash of a URL, used as the disk-cache file stem.
///
/// FNV-1a is fast and deterministic; it is not cryptographic. A collision is
/// harmless only because every entry also records the *request URL* it was
/// fetched for, and that value is checked before a cached response is returned
/// (see [`disk_read`]). The in-memory cache is keyed by the full request URL
/// for the same reason.
pub fn cache_key(url: &Url) -> String {
    format!("{:016x}", fnv1a(url.as_str().as_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Cooked response metadata stored alongside a raw body on disk.
#[derive(Debug, Serialize, Deserialize)]
struct DiskMeta {
    /// The URL that was *requested* (not the post-redirect URL). Verified on
    /// every read so that a hash collision can never serve another URL's body.
    request_url: String,
    status: u16,
    final_url: String,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    /// Hash of the body file, so a partly overwritten/stale body cannot be
    /// paired with metadata from a different response.
    body_hash: u64,
}

/// A tiny insertion-ordered map used as an approximate LRU.
///
/// Entries are keyed by the full request URL, not by [`crate::cache_key`], so
/// that two URLs with the same hash cannot share an entry.
///
/// The front of the map is the least recently used entry. A lookup removes and
/// re-inserts the entry so it moves to the back. `IndexMap::shift_remove` is
/// linear but the cache is intentionally small (a few dozen documents).
pub(crate) struct MemoryCache {
    entries: IndexMap<String, Response>,
    bytes: u64,
    max_bytes: u64,
}

impl MemoryCache {
    pub(crate) fn new(max_bytes: u64) -> Self {
        Self {
            entries: IndexMap::new(),
            bytes: 0,
            max_bytes,
        }
    }

    pub(crate) fn get(&mut self, key: &str) -> Option<Response> {
        let response = self.entries.shift_remove(key)?;
        self.bytes = self.bytes.saturating_sub(entry_size(key, &response));
        self.entries.insert(key.to_owned(), response.clone());
        self.bytes = self.bytes.saturating_add(entry_size(key, &response));
        Some(response)
    }

    pub(crate) fn put(&mut self, key: String, response: Response) {
        if self.max_bytes == 0 {
            return;
        }
        let size = entry_size(&key, &response);
        if size > self.max_bytes {
            return;
        }
        if let Some(old) = self.entries.shift_remove(&key) {
            self.bytes = self.bytes.saturating_sub(entry_size(&key, &old));
        }
        self.entries.insert(key, response);
        self.bytes = self.bytes.saturating_add(size);
        while self.bytes > self.max_bytes {
            match self.entries.shift_remove_index(0) {
                Some((old_key, old)) => {
                    self.bytes = self.bytes.saturating_sub(entry_size(&old_key, &old));
                }
                None => break,
            }
        }
    }
}

/// Approximate memory cost of one cached entry, including key and metadata.
fn entry_size(key: &str, response: &Response) -> u64 {
    128_u64
        .saturating_add(key.len() as u64)
        .saturating_add(response.url.as_str().len() as u64)
        .saturating_add(response.body.len() as u64)
        .saturating_add(
            response
                .content_type
                .as_ref()
                .map_or(0, |value| value.len() as u64),
        )
}

/// Whether a comma-separated directive list contains `name`.
///
/// The `=value` part is ignored, so `no-cache="Set-Cookie"` is recognised as
/// `no-cache` (RFC 9111 allows argument values on `no-cache` and `private`).
fn has_directive(list: &str, name: &str) -> bool {
    list.split(',').any(|part| {
        let token = part.split_once('=').map_or(part, |(token, _)| token);
        token.trim().eq_ignore_ascii_case(name)
    })
}

/// Whether a response is eligible for this simple cache.
///
/// Only successful GET responses are cached by the caller; this checks the
/// status code and the common "do not store" directives. Since a cache hit
/// cannot vary on request headers, responses with a `Vary` other than
/// `Accept-Encoding` (already normalised by reqwest's decompression) are not
/// stored at all.
pub(crate) fn is_cacheable(response: &Response) -> bool {
    // A 206 is only valid for the Range request that produced it; replaying it
    // as a full response would return a truncated document.
    if !matches!(
        response.status,
        StatusCode::OK | StatusCode::NON_AUTHORITATIVE_INFORMATION | StatusCode::NO_CONTENT
    ) {
        return false;
    }
    // Replaying a Set-Cookie response from cache would not replay the cookie,
    // so avoid storing it in the first place.
    if response.headers.contains_key(SET_COOKIE) {
        return false;
    }
    for value in response.headers.get_all(VARY).iter() {
        let Ok(text) = value.to_str() else {
            return false;
        };
        if text.split(',').any(|token| {
            let token = token.trim();
            !token.is_empty() && !token.eq_ignore_ascii_case("accept-encoding")
        }) {
            return false;
        }
    }
    for value in response.headers.get_all(CACHE_CONTROL).iter() {
        let Ok(text) = value.to_str() else {
            return false;
        };
        if has_directive(text, "no-store")
            || has_directive(text, "no-cache")
            || has_directive(text, "private")
        {
            return false;
        }
    }
    !response.headers.get_all(PRAGMA).iter().any(|value| {
        value
            .to_str()
            .is_ok_and(|text| has_directive(text, "no-cache"))
    })
}

/// Read an entry from the on-disk cache. Any problem is treated as a miss.
///
/// `request_url` must be the URL the caller wants to fetch: it is compared
/// with the stored request URL so that an FNV-1a collision (or a stale file
/// produced by a redirect) can never return a response for a different URL.
/// The body hash is checked as well, so a partially rewritten entry is a miss.
pub(crate) fn disk_read(dir: Option<&Path>, key: &str, request_url: &Url) -> Option<Response> {
    let dir = dir?;
    let meta_path = dir.join(format!("{key}.meta"));
    let body_path = dir.join(format!("{key}.body"));
    let meta_text = std::fs::read_to_string(meta_path).ok()?;
    let meta: DiskMeta = serde_json::from_str(&meta_text).ok()?;
    if meta.request_url != request_url.as_str() {
        return None;
    }
    let status = StatusCode::from_u16(meta.status).ok()?;
    let final_url = Url::parse(&meta.final_url).ok()?;
    let body = Bytes::from(std::fs::read(body_path).ok()?);
    if fnv1a(&body) != meta.body_hash {
        return None;
    }
    Some(Response {
        url: final_url,
        status,
        body,
        content_type: meta.content_type,
        headers: headers_from_pairs(&meta.headers),
    })
}

/// Write an entry to the on-disk cache. Cache failures are logged, never
/// propagated to the caller: a broken cache must not break browsing.
pub(crate) fn disk_write(dir: Option<&Path>, key: &str, request_url: &Url, response: &Response) {
    let Some(dir) = dir else {
        return;
    };
    if let Err(err) = std::fs::create_dir_all(dir) {
        warn!(%err, dir = %dir.display(), "cannot create disk cache directory");
        return;
    }

    let mut pairs = Vec::new();
    for (name, value) in response.headers.iter() {
        if let Ok(value) = value.to_str() {
            pairs.push((name.as_str().to_owned(), value.to_owned()));
        }
    }
    let meta = DiskMeta {
        request_url: request_url.to_string(),
        status: response.status.as_u16(),
        final_url: response.url.to_string(),
        content_type: response.content_type.clone(),
        headers: pairs,
        body_hash: fnv1a(&response.body),
    };
    let meta_json = match serde_json::to_vec(&meta) {
        Ok(json) => json,
        Err(err) => {
            warn!(%err, "cannot serialise disk cache metadata");
            return;
        }
    };

    let body_path = dir.join(format!("{key}.body"));
    if let Err(err) = std::fs::write(body_path, &response.body) {
        warn!(%err, "cannot write disk cache body");
        return;
    }
    // Write metadata last: a half-written entry without metadata is a miss.
    let meta_path = dir.join(format!("{key}.meta"));
    if let Err(err) = std::fs::write(meta_path, meta_json) {
        warn!(%err, "cannot write disk cache metadata");
    }
}

fn headers_from_pairs(pairs: &[(String, String)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::{MemoryCache, cache_key, disk_read, disk_write, headers_from_pairs, is_cacheable};
    use bytes::Bytes;
    use reqwest::{
        StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, HeaderMap, HeaderValue, PRAGMA, SET_COOKIE, VARY},
    };
    use url::Url;

    use crate::Response;

    fn response(url: &str, body: &str) -> Response {
        Response {
            url: Url::parse(url).expect("url"),
            status: StatusCode::OK,
            body: Bytes::from(body.to_owned()),
            content_type: None,
            headers: HeaderMap::new(),
        }
    }

    #[test]
    fn hashes_are_stable_and_discriminating() {
        let a = url::Url::parse("https://example.com/page?q=1").expect("valid url");
        let b = url::Url::parse("https://example.com/page?q=2").expect("valid url");
        let a_again = url::Url::parse("https://example.com/page?q=1").expect("valid url");

        let key_a = cache_key(&a);
        assert_eq!(key_a, cache_key(&a_again));
        assert_ne!(key_a, cache_key(&b));
        assert_eq!(key_a.len(), 16);
        assert!(key_a.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn header_pairs_round_trip() {
        let pairs = vec![
            ("content-type".to_owned(), "text/html".to_owned()),
            ("x-bad".to_owned(), "not a real header".to_owned()),
        ];
        let headers = headers_from_pairs(&pairs);
        assert_eq!(
            headers.get(CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/html"))
        );
    }

    #[test]
    fn is_cacheable_honours_directives_and_vary() {
        let mut response = response("https://example.com/page", "body");
        assert!(is_cacheable(&response));

        response
            .headers
            .insert(CACHE_CONTROL, HeaderValue::from_static("max-age=60"));
        assert!(is_cacheable(&response));

        // Directives with arguments must still be recognised.
        response.headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static("no-cache=\"Set-Cookie\""),
        );
        assert!(!is_cacheable(&response));

        response
            .headers
            .insert(CACHE_CONTROL, HeaderValue::from_static("private=\"token\""));
        assert!(!is_cacheable(&response));

        response.headers = HeaderMap::new();
        response
            .headers
            .insert(PRAGMA, HeaderValue::from_static("no-cache, foo"));
        assert!(!is_cacheable(&response));

        response.headers = HeaderMap::new();
        response
            .headers
            .insert(VARY, HeaderValue::from_static("Cookie"));
        assert!(!is_cacheable(&response));

        response
            .headers
            .insert(VARY, HeaderValue::from_static("Accept-Encoding"));
        assert!(is_cacheable(&response));

        response.headers = HeaderMap::new();
        response
            .headers
            .insert(SET_COOKIE, HeaderValue::from_static("a=b"));
        assert!(!is_cacheable(&response));

        response.headers = HeaderMap::new();
        response.status = StatusCode::PARTIAL_CONTENT;
        assert!(!is_cacheable(&response));
    }

    #[test]
    fn disk_read_verifies_request_url_and_keeps_final_url() {
        let dir = std::env::temp_dir().join(format!("mil-net-disk-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Url::parse("https://one.example/page").expect("url");
        let other = Url::parse("https://two.example/page").expect("url");
        // Same key simulates an FNV-1a collision: the other URL must miss.
        let key = cache_key(&first);
        let cached = response("https://redirected.example/final", "secret");
        disk_write(Some(&dir), &key, &first, &cached);

        assert!(disk_read(Some(&dir), &key, &other).is_none());
        let hit = disk_read(Some(&dir), &key, &first).expect("cache hit");
        assert_eq!(&hit.body[..], b"secret");
        assert_eq!(hit.url.as_str(), "https://redirected.example/final");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disk_read_rejects_tampered_body() {
        let dir = std::env::temp_dir().join(format!("mil-net-disk-body-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = Url::parse("https://example.com/page").expect("url");
        let key = cache_key(&url);
        disk_write(
            Some(&dir),
            &key,
            &url,
            &response("https://example.com/page", "original"),
        );

        std::fs::write(dir.join(format!("{key}.body")), b"tampered").expect("overwrite body");
        assert!(disk_read(Some(&dir), &key, &url).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn memory_cache_evicts_least_recently_used_entry() {
        let mut cache = MemoryCache::new(300);
        let a = response("https://a.test/", "a");
        let b = response("https://b.test/", "b");
        let c = response("https://c.test/", "c");

        cache.put("a".to_owned(), a);
        cache.put("b".to_owned(), b);
        assert!(cache.get("a").is_some()); // "a" becomes most recently used
        cache.put("c".to_owned(), c);

        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert!(cache.get("b").is_none());
    }

    #[test]
    fn oversized_entries_are_not_cached() {
        let mut cache = MemoryCache::new(64);
        let big_body = "x".repeat(100);
        cache.put("big".to_owned(), response("https://a.test/", &big_body));
        assert!(cache.get("big").is_none());
    }
}
