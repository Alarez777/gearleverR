//! Error type shared by the whole core crate.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Largest `Retry-After` value honoured by the update-check retry pass.
///
/// Waiting longer than this would freeze the check for no useful gain: a
/// primary GitHub rate limit resets in up to an hour and a token is the right
/// fix, while secondary limits usually ask for a few seconds.
const MAX_RETRY_AFTER: u64 = 5;

/// A non-success HTTP response and its rate-limit metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpStatus {
    /// URL that returned the response.
    pub url: String,
    /// HTTP status code.
    pub status: u16,
    /// Remaining requests in the current GitHub rate-limit window.
    pub rate_limit_remaining: Option<u64>,
    /// Unix timestamp when the GitHub rate-limit window resets.
    pub rate_limit_reset: Option<u64>,
    /// Seconds the server asked the client to wait before retrying.
    pub retry_after: Option<u64>,
}

impl fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: HTTP {}", self.url, self.status)?;
        if let Some(remaining) = self.rate_limit_remaining {
            write!(f, " (rate limit remaining: {remaining})")?;
        }
        if let Some(reset) = self.rate_limit_reset {
            write!(f, " (resets at {reset})")?;
        }
        if let Some(retry_after) = self.retry_after {
            write!(f, " (retry after {retry_after}s)")?;
        }
        Ok(())
    }
}

/// Errors returned by Gear LeverR's core.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O operation failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The file does not start with an AppImage runtime magic.
    #[error("{path}: not an AppImage (magic {magic:02x?})")]
    NotAnAppImage {
        /// File that was inspected.
        path: PathBuf,
        /// Bytes at offset 8 of the file.
        magic: [u8; 3],
    },

    /// The ELF runtime header is malformed or truncated.
    #[error("invalid ELF header: {0}")]
    InvalidElf(&'static str),

    /// The embedded payload is a format this build cannot read yet.
    #[error("{0}: unsupported AppImage payload")]
    UnsupportedPayload(PathBuf),

    /// The SquashFS payload could not be parsed.
    #[error("cannot read the embedded SquashFS image: {0}")]
    Squashfs(#[from] backhand::BackhandError),

    /// The AppImage contains no usable desktop entry.
    #[error("no .desktop file found inside {0}")]
    MissingDesktopEntry(PathBuf),

    /// A desktop entry is not valid UTF-8.
    #[error("{0}: desktop entry is not valid UTF-8")]
    DesktopEntryNotUtf8(PathBuf),

    /// `HOME` is not set, so XDG paths cannot be resolved.
    #[error("cannot determine the home directory: HOME is not set")]
    NoHomeDirectory,

    /// A desktop entry `Exec` line cannot be parsed.
    #[error("invalid command line: {0}")]
    InvalidCommandLine(String),

    /// A cached value cannot be decoded.
    #[error("invalid cache entry: {0}")]
    Cache(#[from] serde_json::Error),

    /// A path cannot be used to derive a file or application name.
    #[error("{0}: cannot derive a name from this path")]
    InvalidName(PathBuf),

    /// A network request failed or returned an unexpected status.
    #[error("network error: {0}")]
    Http(String),

    /// A network request returned a non-success HTTP status.
    #[error("network error: {0}")]
    HttpStatus(HttpStatus),

    /// GitHub rejected the configured token.
    #[error("GitHub authentication failed (401). Check your token in Preferences → GitHub.")]
    GithubTokenInvalid,

    /// An update manager is missing required configuration.
    #[error("invalid update configuration: {0}")]
    UpdateConfig(String),

    /// The user cancelled the operation.
    #[error("operation cancelled")]
    Cancelled,

    /// No external tool can extract this AppImage's payload.
    #[error("{0}: no extractor available, install 7zz or dwarfs and retry")]
    MissingExtractor(PathBuf),

    /// No release matches the configured file name.
    #[error("no release asset matches {0}")]
    NoMatchingAsset(String),
}

impl Error {
    /// Returns how long to wait before retrying, or `None` when a retry cannot
    /// change the result.
    ///
    /// Transport errors, timeouts and 5xx responses are retried immediately.
    /// A 403 or 429 is retried only when GitHub sends a short `Retry-After`;
    /// 401, 404 and permission errors are deterministic.
    #[must_use]
    pub fn retry_delay(&self) -> Option<Duration> {
        match self {
            Self::Http(_) => Some(Duration::ZERO),
            Self::HttpStatus(status) if status.status == 408 || status.status >= 500 => {
                Some(Duration::ZERO)
            }
            Self::HttpStatus(status) if matches!(status.status, 403 | 429) => status
                .retry_after
                .filter(|seconds| *seconds <= MAX_RETRY_AFTER)
                .map(Duration::from_secs),
            _ => None,
        }
    }
}

/// Convenient alias for the crate's [`Result`].
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16, remaining: Option<u64>, retry_after: Option<u64>) -> Error {
        Error::HttpStatus(HttpStatus {
            url: "https://api.github.com/repos/o/r".to_owned(),
            status: code,
            rate_limit_remaining: remaining,
            rate_limit_reset: None,
            retry_after,
        })
    }

    #[test]
    fn retries_transient_failures() {
        assert_eq!(status(500, None, None).retry_delay(), Some(Duration::ZERO));
        assert_eq!(status(408, None, None).retry_delay(), Some(Duration::ZERO));
        assert_eq!(
            Error::Http("timed out".to_owned()).retry_delay(),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn does_not_retry_deterministic_failures() {
        assert_eq!(status(401, None, None).retry_delay(), None);
        assert_eq!(status(404, None, None).retry_delay(), None);
        assert_eq!(status(403, Some(0), None).retry_delay(), None);
        assert_eq!(status(403, Some(42), None).retry_delay(), None);
        assert_eq!(Error::GithubTokenInvalid.retry_delay(), None);
    }

    #[test]
    fn honours_a_short_retry_after() {
        assert_eq!(
            status(429, None, Some(3)).retry_delay(),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            status(403, Some(0), Some(2)).retry_delay(),
            Some(Duration::from_secs(2))
        );
        assert_eq!(status(429, None, Some(30)).retry_delay(), None);
    }
}
