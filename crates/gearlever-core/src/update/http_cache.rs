//! Persistent cache for conditional HTTP requests.
//!
//! Update checks download release metadata and `.zsync` headers repeatedly.
//! When the server sends an `ETag` or `Last-Modified`, the next request can be
//! conditional and a `304 Not Modified` reuses the stored body. A short
//! in-process freshness window also coalesces concurrent checks of the same URL.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// On-disk format version. Bump it when [`CacheEntry`] changes.
const CACHE_VERSION: u32 = 2;

/// Largest response body kept in the cache.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Entries older than this are ignored and removed.
const MAX_AGE_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Default window for reusing a response without revalidating it.
const COALESCE_WINDOW: Duration = Duration::from_secs(2);

/// One cached HTTP response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CacheEntry {
    version: u32,
    #[serde(with = "base64_bytes")]
    pub(crate) body: Vec<u8>,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    fetched_at: u64,
}

impl CacheEntry {
    /// Builds an entry stamped with the current time.
    pub(crate) fn new(body: Vec<u8>, etag: Option<String>, last_modified: Option<String>) -> Self {
        Self {
            version: CACHE_VERSION,
            body,
            etag,
            last_modified,
            fetched_at: now_seconds(),
        }
    }

    fn is_usable(&self) -> bool {
        self.version == CACHE_VERSION
            && now_seconds().saturating_sub(self.fetched_at) <= MAX_AGE_SECONDS
    }

    fn is_fresh(&self, window: Duration) -> bool {
        now_seconds().saturating_sub(self.fetched_at) < window.as_secs()
    }
}

/// A disk-backed HTTP cache with per-key request coalescing.
#[derive(Debug)]
pub(crate) struct HttpCache {
    root: PathBuf,
    window: Duration,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl HttpCache {
    /// Creates a cache rooted at `root`.
    pub(crate) fn new(root: PathBuf) -> Self {
        Self::with_window(root, COALESCE_WINDOW)
    }

    /// Creates a cache with an explicit coalescing window.
    ///
    /// Tests use [`Duration::ZERO`] to force revalidation between requests.
    pub(crate) fn with_window(root: PathBuf, window: Duration) -> Self {
        let _ = secure_directory(&root);
        Self {
            root,
            window,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the cache key for a request.
    pub(crate) fn key(url: &str, accept: Option<&str>, token: Option<&str>) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"gearlever-r-http-v2\0");
        hasher.update(url.as_bytes());
        hasher.update(b"\0");
        if let Some(accept) = accept {
            hasher.update(accept.as_bytes());
        }
        hasher.update(b"\0");
        hasher.update(token_scope(token).as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Returns the lock that serializes requests for `key`.
    pub(crate) fn lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().expect("http cache lock poisoned");
        Arc::clone(
            locks
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Returns the cached body when it was fetched within the freshness window.
    pub(crate) fn fresh(&self, key: &str) -> Option<Vec<u8>> {
        let entry = self.load(key)?;
        entry.is_fresh(self.window).then_some(entry.body)
    }

    /// Loads an entry from disk, ignoring corrupt or expired files.
    pub(crate) fn load(&self, key: &str) -> Option<CacheEntry> {
        let path = self.path(key);
        let contents = fs::read_to_string(&path).ok()?;
        let entry: CacheEntry = if let Ok(entry) = serde_json::from_str(&contents) {
            entry
        } else {
            let _ = fs::remove_file(&path);
            return None;
        };
        if !entry.is_usable() {
            let _ = fs::remove_file(&path);
            return None;
        }
        Some(entry)
    }

    /// Writes an entry atomically, ignoring cache failures.
    pub(crate) fn store(&self, key: &str, entry: &CacheEntry) {
        let path = self.path(key);
        if entry.body.len() > MAX_BODY_BYTES {
            let _ = fs::remove_file(&path);
            return;
        }

        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let _ = secure_directory(parent);

        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let Ok(serialized) = serde_json::to_vec(entry) else {
            return;
        };
        if fs::write(&temporary, serialized).is_err() {
            let _ = fs::remove_file(&temporary);
            return;
        }
        let _ = secure_file(&temporary);
        if fs::rename(&temporary, &path).is_err() {
            let _ = fs::remove_file(&temporary);
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn token_scope(token: Option<&str>) -> String {
    token.map_or_else(
        || "anonymous".to_owned(),
        |token| blake3::hash(token.as_bytes()).to_hex().to_string(),
    )
}

fn secure_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn secure_file(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        STANDARD.decode(encoded).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &[u8]) -> CacheEntry {
        CacheEntry::new(
            body.to_vec(),
            Some("\"v1\"".to_owned()),
            Some("Thu, 01 Jan 2026 00:00:00 GMT".to_owned()),
        )
    }

    #[test]
    fn stores_and_loads_entries() {
        let directory = tempfile::tempdir().unwrap();
        let cache = HttpCache::new(directory.path().join("http"));
        let key = HttpCache::key(
            "https://example.com/releases",
            Some("application/json"),
            None,
        );

        cache.store(&key, &entry(b"hello"));

        let loaded = cache.load(&key).expect("entry is cached");
        assert_eq!(loaded.body, b"hello");
        assert_eq!(loaded.etag.as_deref(), Some("\"v1\""));
    }

    #[test]
    fn round_trips_binary_bodies() {
        let directory = tempfile::tempdir().unwrap();
        let cache = HttpCache::new(directory.path().join("http"));
        let body = [0x00, 0xFF, 0xFE, 0x80, b'h', b'i'];

        cache.store("control", &entry(&body));

        assert_eq!(cache.load("control").unwrap().body, body);
    }

    #[test]
    fn keys_are_scoped_by_token_and_accept() {
        let anonymous = HttpCache::key("https://example.com", None, None);
        let token_a = HttpCache::key("https://example.com", None, Some("token-a"));
        let token_b = HttpCache::key("https://example.com", None, Some("token-b"));
        let json = HttpCache::key(
            "https://example.com",
            Some("application/json"),
            Some("token-a"),
        );

        assert_ne!(anonymous, token_a);
        assert_ne!(token_a, token_b);
        assert_ne!(token_a, json);
    }

    #[test]
    fn freshness_window_controls_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let key = "entry";

        let cache = HttpCache::with_window(directory.path().join("http"), Duration::ZERO);
        cache.store(key, &entry(b"hello"));
        assert_eq!(cache.fresh(key), None, "zero window always revalidates");

        let cache = HttpCache::with_window(directory.path().join("http"), Duration::MAX);
        cache.store(key, &entry(b"hello"));
        assert_eq!(cache.fresh(key).as_deref(), Some(b"hello".as_slice()));
    }

    #[test]
    fn rejects_bodies_that_do_not_fit() {
        let directory = tempfile::tempdir().unwrap();
        let cache = HttpCache::new(directory.path().join("http"));
        let key = "large";

        cache.store(key, &entry(&vec![b'x'; MAX_BODY_BYTES + 1]));

        assert!(cache.load(key).is_none());
    }
}
