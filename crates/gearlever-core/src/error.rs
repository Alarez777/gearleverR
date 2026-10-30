//! Error type shared by the whole core crate.

use std::path::PathBuf;

/// Errors returned by Gear Lever R's core.
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

    /// An update manager is missing required configuration.
    #[error("invalid update configuration: {0}")]
    UpdateConfig(String),

    /// No release matches the configured file name.
    #[error("no release asset matches {0}")]
    NoMatchingAsset(String),
}

/// Convenient alias for the crate's [`Result`].
pub type Result<T> = std::result::Result<T, Error>;
