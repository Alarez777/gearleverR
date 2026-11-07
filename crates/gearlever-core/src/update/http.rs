//! Small blocking HTTP client built on `ureq` with rustls.
//!
//! Gear LeverR never links OpenSSL: the AppImage must run on any distribution
//! without extra system packages.

use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::error::{Error, HttpStatus, Result};
use crate::paths::Paths;

use super::http_cache::{CacheEntry, HttpCache};

const USER_AGENT: &str = concat!("GearLeverR/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// A blocking HTTP client shared by all update sources.
#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
    cache: Option<Arc<HttpCache>>,
}

struct BytesResponse {
    body: Vec<u8>,
    headers: http::HeaderMap,
    not_modified: bool,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpClient")
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    /// Creates a client with a user agent and connect, response and read
    /// timeouts.
    ///
    /// There is deliberately no global timeout: a large AppImage may take
    /// longer than a fixed budget, while a stalled read still fails.
    #[must_use]
    pub fn new() -> Self {
        Self {
            agent: Self::build_agent(),
            cache: None,
        }
    }

    fn build_agent() -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .timeout_per_read(Some(READ_TIMEOUT))
            .http_status_as_error(false)
            .build();

        ureq::Agent::new_with_config(config)
    }

    /// Returns a clone of the agent, sharing its connection pool.
    pub(crate) fn agent(&self) -> ureq::Agent {
        self.agent.clone()
    }

    /// Creates a client with a persistent HTTP cache.
    #[must_use]
    pub(crate) fn with_cache(cache: HttpCache) -> Self {
        Self {
            agent: Self::build_agent(),
            cache: Some(Arc::new(cache)),
        }
    }

    /// Returns the shared client used by update checks and downloads.
    ///
    /// Cloning it shares the connection pool and the HTTP cache, so checks
    /// against the same URL reuse both the TLS connection and the response.
    #[must_use]
    pub fn shared() -> Self {
        static SHARED: OnceLock<HttpClient> = OnceLock::new();

        SHARED
            .get_or_init(|| match Paths::from_env() {
                Ok(paths) => Self::with_cache(HttpCache::new(paths.cache_root().join("http"))),
                Err(_) => Self::new(),
            })
            .clone()
    }

    /// Fetches a text document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for transport errors and non-success statuses.
    pub fn get_text(&self, url: &str) -> Result<String> {
        self.get_text_with_accept(url, None)
    }

    /// Fetches a text document with an optional `Accept` header.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for transport errors and
    /// [`Error::HttpStatus`] for non-success statuses.
    pub fn get_text_with_accept(&self, url: &str, accept: Option<&str>) -> Result<String> {
        self.get_text_with_auth(url, accept, None)
    }

    /// Fetches a binary document.
    ///
    /// The body is cached when the client has a cache, so a later delta
    /// download can reuse the `.zsync` control file fetched by the check.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for transport errors and
    /// [`Error::HttpStatus`] for non-success statuses.
    pub(crate) fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        self.get_bytes_with_auth(url, None, None)
    }

    /// Fetches a text document with optional `Accept` and `Authorization`
    /// headers.
    ///
    /// The token is only added when the caller passes one; update sources are
    /// responsible for using it exclusively against their own API host.
    fn get_text_with_auth(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<String> {
        let body = self.get_bytes_with_auth(url, accept, token)?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Fetches a binary document with optional `Accept` and `Authorization`
    /// headers.
    ///
    /// When the client has a cache, the request is conditional and concurrent
    /// calls for the same URL are coalesced.
    ///
    /// The token is only added when the caller passes one; update sources are
    /// responsible for using it exclusively against their own API host.
    fn get_bytes_with_auth(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<Vec<u8>> {
        let Some(cache) = &self.cache else {
            return self
                .send_bytes(url, accept, token, None)
                .map(|response| response.body);
        };

        let key = HttpCache::key(url, accept, token);
        let lock = cache.lock(&key);
        let _guard = lock.lock().expect("http cache lock poisoned");

        if let Some(body) = cache.fresh(&key) {
            return Ok(body);
        }

        let cached = cache.load(&key);
        let response = self.send_bytes(url, accept, token, cached.as_ref())?;
        let (etag, last_modified) = if response.not_modified {
            (
                header_value(&response.headers, "etag")
                    .or_else(|| cached.as_ref().and_then(|entry| entry.etag.clone())),
                header_value(&response.headers, "last-modified").or_else(|| {
                    cached
                        .as_ref()
                        .and_then(|entry| entry.last_modified.clone())
                }),
            )
        } else {
            (
                header_value(&response.headers, "etag"),
                header_value(&response.headers, "last-modified"),
            )
        };
        cache.store(
            &key,
            &CacheEntry::new(response.body.clone(), etag, last_modified),
        );

        Ok(response.body)
    }

    fn get_text_with_auth_response(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<(String, http::HeaderMap)> {
        let response = self.send_bytes(url, accept, token, None)?;
        Ok((
            String::from_utf8_lossy(&response.body).into_owned(),
            response.headers,
        ))
    }

    fn send_bytes(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
        cached: Option<&CacheEntry>,
    ) -> Result<BytesResponse> {
        let mut request = self.agent.get(url);
        if let Some(accept) = accept {
            request = request.header("Accept", accept);
        }
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(cached) = cached {
            if let Some(etag) = &cached.etag {
                request = request.header("If-None-Match", etag);
            }
            if let Some(last_modified) = &cached.last_modified {
                request = request.header("If-Modified-Since", last_modified);
            }
        }

        let response = request
            .call()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;
        let status = response.status();

        if status.as_u16() == 304 {
            let Some(cached) = cached else {
                return Err(Error::Http(format!(
                    "{url}: 304 Not Modified without a cached response"
                )));
            };
            return Ok(BytesResponse {
                body: cached.body.clone(),
                headers: response.headers().clone(),
                not_modified: true,
            });
        }

        if !status.is_success() {
            return Err(Error::HttpStatus(http_status(
                url,
                status.as_u16(),
                response.headers(),
            )));
        }

        let headers = response.headers().clone();
        let mut body = response.into_body();
        let bytes = body
            .read_to_vec()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;

        Ok(BytesResponse {
            body: bytes,
            headers,
            not_modified: false,
        })
    }

    /// Fetches and decodes a JSON document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for transport errors,
    /// [`Error::InvalidResponse`] for invalid JSON, and [`Error::HttpStatus`]
    /// for non-success statuses.
    pub fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        accept: Option<&str>,
    ) -> Result<T> {
        self.get_json_with_token(url, accept, None)
    }

    /// Fetches and decodes a JSON document, optionally authenticating with a
    /// bearer token.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for transport errors,
    /// [`Error::InvalidResponse`] for invalid JSON, and [`Error::HttpStatus`]
    /// for non-success statuses.
    pub fn get_json_with_token<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<T> {
        let body = self.get_text_with_auth(url, accept, token)?;
        serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
            url: url.to_owned(),
            error: error.to_string(),
        })
    }

    /// Fetches and decodes a JSON document, returning the GitHub token
    /// expiration header when the response carries one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for transport errors,
    /// [`Error::InvalidResponse`] for invalid JSON, and [`Error::HttpStatus`]
    /// for non-success statuses.
    pub fn get_json_with_token_expiration<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<(T, Option<String>)> {
        let (body, headers) = self.get_text_with_auth_response(url, accept, token)?;
        let value = serde_json::from_str(&body).map_err(|error| Error::InvalidResponse {
            url: url.to_owned(),
            error: error.to_string(),
        })?;

        Ok((value, token_expiration(&headers)))
    }

    /// Returns the `Content-Length` of `url`, using HEAD and falling back to GET.
    ///
    /// A transport error is not retried: a server that accepted the connection
    /// but never answered would otherwise run the whole timeout twice.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] when the URL cannot be resolved.
    pub fn content_length(&self, url: &str) -> Result<Option<u64>> {
        match self.agent.head(url).call() {
            Ok(response) if response.status().is_success() => {
                return Ok(content_length(response.headers()));
            }
            Ok(_) => {}
            Err(error) => {
                return Err(Error::Http(format!("{url}: {error}")));
            }
        }

        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::HttpStatus(http_status(
                url,
                status.as_u16(),
                response.headers(),
            )));
        }

        Ok(content_length(response.headers()))
    }

    /// Streams `url` into `destination`, reporting progress between 0.0 and 1.0.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for HTTP failures and [`Error::Io`] when the file
    /// cannot be written.
    pub fn download(
        &self,
        url: &str,
        destination: &Path,
        progress: &mut dyn FnMut(f64),
        cancel: &AtomicBool,
    ) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }

        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::HttpStatus(http_status(
                url,
                status.as_u16(),
                response.headers(),
            )));
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let total = content_length(response.headers());
        let mut body = response.into_body();
        let mut reader = body.as_reader();
        let mut file = BufWriter::new(File::create(destination)?);
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut written = 0_u64;
        let mut last_reported = 0.0_f64;

        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = std::fs::remove_file(destination);
                return Err(Error::Cancelled);
            }

            let read = reader
                .read(&mut buffer)
                .map_err(|error| Error::Http(format!("{url}: {error}")))?;
            if read == 0 {
                break;
            }

            file.write_all(&buffer[..read])?;
            written = written.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));

            if let Some(total) = total
                && total > 0
            {
                let fraction = fraction(written, total);
                if fraction - last_reported >= 0.01 {
                    last_reported = fraction;
                    progress(fraction);
                }
            }
        }

        file.flush()?;
        progress(1.0);

        if let Some(total) = total
            && written < total
        {
            return Err(Error::Http(format!(
                "{url}: incomplete download ({written} of {total} bytes)"
            )));
        }

        Ok(())
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "download progress does not need exact precision"
)]
fn fraction(written: u64, total: u64) -> f64 {
    written as f64 / total as f64
}

fn header_value(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn content_length(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

/// Builds an [`HttpStatus`] from a response, including the GitHub rate-limit
/// headers used by the retry policy.
fn http_status(url: &str, status: u16, headers: &http::HeaderMap) -> HttpStatus {
    HttpStatus {
        url: url.to_owned(),
        status,
        rate_limit_remaining: header_u64(headers, "x-ratelimit-remaining"),
        rate_limit_reset: header_u64(headers, "x-ratelimit-reset"),
        retry_after: header_u64(headers, "retry-after"),
    }
}

fn header_u64(headers: &http::HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

fn token_expiration(headers: &http::HeaderMap) -> Option<String> {
    headers
        .get("github-authentication-token-expiration")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    type TestResponse = (u16, Vec<(&'static str, String)>, Vec<u8>);

    fn serve<F>(
        handler: F,
    ) -> (
        String,
        Arc<AtomicBool>,
        Arc<AtomicUsize>,
        std::thread::JoinHandle<()>,
    )
    where
        F: Fn(String) -> TestResponse + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));

        let server = {
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                            let mut buffer = [0_u8; 8192];
                            let read = stream.read(&mut buffer).unwrap_or(0);
                            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                            requests.fetch_add(1, Ordering::SeqCst);

                            let (status, headers, body) = handler(request);
                            let reason = match status {
                                200 => "OK",
                                304 => "Not Modified",
                                _ => "Error",
                            };
                            let mut response = format!(
                                "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n",
                                body.len()
                            );
                            for (name, value) in headers {
                                let _ = write!(response, "{name}: {value}\r\n");
                            }
                            response.push_str("\r\n");
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.write_all(&body);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        (format!("http://{address}/resource"), stop, requests, server)
    }

    #[test]
    fn parses_content_length_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert("content-length", http::HeaderValue::from_static("1234"));

        assert_eq!(content_length(&headers), Some(1234));
    }

    #[test]
    fn reads_github_rate_limit_headers() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-ratelimit-remaining", http::HeaderValue::from_static("0"));
        headers.insert(
            "x-ratelimit-reset",
            http::HeaderValue::from_static("1700000000"),
        );
        headers.insert("retry-after", http::HeaderValue::from_static("3"));

        let status = http_status("https://api.github.com/repos/o/r", 403, &headers);

        assert_eq!(status.rate_limit_remaining, Some(0));
        assert_eq!(status.rate_limit_reset, Some(1_700_000_000));
        assert_eq!(status.retry_after, Some(3));
    }

    #[test]
    fn sends_a_bearer_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();

            assert!(
                request.contains("authorization: bearer test-token"),
                "request had no bearer token: {request}"
            );

            let body = r#"{"resources":{"core":{"limit":5000,"remaining":4999}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ngithub-authentication-token-expiration: 2026-11-10 12:00:00 +0000\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let client = HttpClient::new();
        let (value, expires_at): (serde_json::Value, _) = client
            .get_json_with_token_expiration(
                &format!("http://{address}/rate_limit"),
                None,
                Some("test-token"),
            )
            .unwrap();

        assert_eq!(value["resources"]["core"]["remaining"], 4999);
        assert_eq!(expires_at.as_deref(), Some("2026-11-10 12:00:00 +0000"));
        server.join().unwrap();
    }

    #[test]
    fn reports_invalid_json_as_an_invalid_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            let body = "{}";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let client = HttpClient::new();
        let error = client
            .get_json::<Vec<serde_json::Value>>(&format!("http://{address}/releases/latest"), None)
            .unwrap_err();

        assert!(matches!(error, Error::InvalidResponse { .. }), "{error}");
        assert_eq!(error.retry_delay(), None);
        server.join().unwrap();
    }

    #[test]
    fn revalidates_with_etag_and_reuses_the_cached_body() {
        let (url, stop, requests, server) = serve(|request| {
            if request
                .to_ascii_lowercase()
                .contains("if-none-match: \"v1\"")
            {
                (304, vec![("ETag", "\"v1\"".to_owned())], Vec::new())
            } else {
                (
                    200,
                    vec![("ETag", "\"v1\"".to_owned())],
                    br#"{"value":1}"#.to_vec(),
                )
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let client = HttpClient::with_cache(HttpCache::with_window(
            directory.path().join("http"),
            Duration::ZERO,
        ));

        let first: serde_json::Value = client.get_json(&url, None).unwrap();
        let second: serde_json::Value = client.get_json(&url, None).unwrap();

        assert_eq!(first, second);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn coalesces_concurrent_requests_for_the_same_url() {
        let (url, stop, requests, server) = serve(|_| {
            std::thread::sleep(Duration::from_millis(200));
            (
                200,
                vec![("ETag", "\"v1\"".to_owned())],
                br#"{"value":1}"#.to_vec(),
            )
        });
        let directory = tempfile::tempdir().unwrap();
        let client = HttpClient::with_cache(HttpCache::with_window(
            directory.path().join("http"),
            Duration::from_secs(30),
        ));
        let barrier = Arc::new(std::sync::Barrier::new(4));

        std::thread::scope(|scope| {
            for _ in 0..4 {
                let client = client.clone();
                let url = url.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    let value: serde_json::Value = client.get_json(&url, None).unwrap();
                    assert_eq!(value["value"], 1);
                });
            }
        });

        assert_eq!(requests.load(Ordering::SeqCst), 1);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn does_not_reuse_the_cache_across_tokens() {
        let (url, stop, requests, server) = serve(|request| {
            let is_a = request
                .to_ascii_lowercase()
                .contains("authorization: bearer token-a");
            if is_a {
                (
                    200,
                    vec![("ETag", "\"a\"".to_owned())],
                    br#"{"token":"a"}"#.to_vec(),
                )
            } else {
                (
                    200,
                    vec![("ETag", "\"b\"".to_owned())],
                    br#"{"token":"b"}"#.to_vec(),
                )
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let client = HttpClient::with_cache(HttpCache::new(directory.path().join("http")));

        let first: serde_json::Value = client
            .get_json_with_token(&url, None, Some("token-a"))
            .unwrap();
        let second: serde_json::Value = client
            .get_json_with_token(&url, None, Some("token-b"))
            .unwrap();

        assert_eq!(first["token"], "a");
        assert_eq!(second["token"], "b");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn token_expiration_requests_bypass_the_cache() {
        let (url, stop, requests, server) = serve(|_| {
            (
                200,
                vec![
                    ("ETag", "\"v1\"".to_owned()),
                    (
                        "github-authentication-token-expiration",
                        "2026-11-10 12:00:00 +0000".to_owned(),
                    ),
                ],
                br#"{"resources":{}}"#.to_vec(),
            )
        });
        let directory = tempfile::tempdir().unwrap();
        let client = HttpClient::with_cache(HttpCache::new(directory.path().join("http")));

        for _ in 0..2 {
            let (value, expires_at): (serde_json::Value, _) = client
                .get_json_with_token_expiration(&url, None, Some("test-token"))
                .unwrap();
            assert!(value.get("resources").is_some());
            assert_eq!(expires_at.as_deref(), Some("2026-11-10 12:00:00 +0000"));
        }

        assert_eq!(requests.load(Ordering::SeqCst), 2);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }
}
