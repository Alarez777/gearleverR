//! Small blocking HTTP client built on `ureq` with rustls.
//!
//! Gear LeverR never links OpenSSL: the AppImage must run on any distribution
//! without extra system packages.

use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::error::{Error, HttpStatus, Result};

const USER_AGENT: &str = concat!("GearLeverR/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// A blocking HTTP client shared by all update sources.
#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
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
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .timeout_per_read(Some(READ_TIMEOUT))
            .http_status_as_error(false)
            .build();

        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// Returns the shared client used by update checks and downloads.
    ///
    /// Cloning it shares the connection pool, so checks against the same host
    /// reuse the existing TLS connection.
    #[must_use]
    pub fn shared() -> Self {
        static SHARED: OnceLock<HttpClient> = OnceLock::new();

        SHARED.get_or_init(Self::new).clone()
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
        let mut request = self.agent.get(url);
        if let Some(accept) = accept {
            request = request.header("Accept", accept);
        }
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        let response = request
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

        let mut body = response.into_body();
        let bytes = body
            .read_to_vec()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;

        // zsync control files carry binary checksums after a text header.
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Fetches and decodes a JSON document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] for transport errors and invalid JSON, and
    /// [`Error::HttpStatus`] for non-success statuses.
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
    /// Returns [`Error::Http`] for transport errors and invalid JSON, and
    /// [`Error::HttpStatus`] for non-success statuses.
    pub fn get_json_with_token<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<T> {
        let body = self.get_text_with_auth(url, accept, token)?;
        serde_json::from_str(&body).map_err(|error| Error::Http(format!("{url}: {error}")))
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

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

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
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let client = HttpClient::new();
        let value: serde_json::Value = client
            .get_json_with_token(
                &format!("http://{address}/rate_limit"),
                None,
                Some("test-token"),
            )
            .unwrap();

        assert_eq!(value["resources"]["core"]["remaining"], 4999);
        server.join().unwrap();
    }
}
