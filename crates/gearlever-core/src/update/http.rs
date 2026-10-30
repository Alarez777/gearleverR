//! Small blocking HTTP client built on `ureq` with rustls.
//!
//! Gear Lever R never links OpenSSL: the AppImage must run on any distribution
//! without extra system packages.

use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};

const USER_AGENT: &str = concat!("GearLeverR/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Creates a client with a user agent and a global timeout.
    #[must_use]
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build();

        Self {
            agent: ureq::Agent::new_with_config(config),
        }
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
    /// Returns [`Error::Http`] for transport errors and non-success statuses.
    pub fn get_text_with_accept(&self, url: &str, accept: Option<&str>) -> Result<String> {
        let mut request = self.agent.get(url);
        if let Some(accept) = accept {
            request = request.header("Accept", accept);
        }

        let response = request
            .call()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;
        let status = response.status();

        if !status.is_success() {
            return Err(Error::Http(format!("{url}: HTTP {status}")));
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
    /// Returns [`Error::Http`] for transport errors, non-success statuses and
    /// invalid JSON.
    pub fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        accept: Option<&str>,
    ) -> Result<T> {
        let body = self.get_text_with_accept(url, accept)?;
        serde_json::from_str(&body).map_err(|error| Error::Http(format!("{url}: {error}")))
    }

    /// Returns the `Content-Length` of `url`, using HEAD and falling back to GET.
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
                tracing::debug!(%url, %error, "HEAD failed, retrying with GET");
            }
        }

        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http(format!("{url}: HTTP {status}")));
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
    ) -> Result<()> {
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| Error::Http(format!("{url}: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http(format!("{url}: HTTP {status}")));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert("content-length", http::HeaderValue::from_static("1234"));

        assert_eq!(content_length(&headers), Some(1234));
    }
}
