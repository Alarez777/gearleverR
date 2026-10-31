//! File hashing with an optional persistent cache.
//!
//! Update checks compare a SHA-1 or SHA-256 digest of the installed AppImage
//! with the one published by the developer. Hashing a 200 MB file on every
//! check would be wasteful, so [`HashCache`] keeps the last digest and
//! invalidates it when size or modification time changes.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::Sha256;

use md5::{Digest, Md5};

use crate::error::{Error, Result};

/// Supported digest algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Algorithm {
    /// MD5, used only to display a fingerprint.
    Md5,
    /// SHA-1, used by zsync update information.
    Sha1,
    /// SHA-256, used by GitHub release digests.
    Sha256,
}

impl Algorithm {
    /// Returns the lower-case name used in cache entries and messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
}

/// Computes the digest of a whole file.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be read.
pub fn hash_file(path: &Path, algorithm: Algorithm) -> Result<String> {
    match algorithm {
        Algorithm::Md5 => digest_file::<Md5>(path),
        Algorithm::Sha1 => digest_file::<Sha1>(path),
        Algorithm::Sha256 => digest_file::<Sha256>(path),
    }
}

fn digest_file<D: Digest>(path: &Path) -> Result<String> {
    let mut reader = BufReader::with_capacity(256 * 1024, File::open(path)?);
    let mut hasher = D::new();
    let mut buffer = vec![0_u8; 256 * 1024];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Fingerprint used to decide whether a cached digest is still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Fingerprint {
    pub(crate) size: u64,
    pub(crate) modified_seconds: u64,
    pub(crate) modified_nanos: u32,
}

impl Fingerprint {
    /// Returns the fingerprint of `path`, or `None` when it does not exist.
    pub(crate) fn of(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::Io(error)),
        };

        let elapsed = metadata
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        Ok(Some(Self {
            size: metadata.len(),
            modified_seconds: elapsed.as_secs(),
            modified_nanos: elapsed.subsec_nanos(),
        }))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedHash {
    algorithm: String,
    fingerprint: Fingerprint,
    value: String,
}

/// A persistent SHA-1 cache rooted at a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashCache {
    root: PathBuf,
}

impl HashCache {
    /// Creates a cache rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the SHA-1 of `path`, computing it only when the entry is stale.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file cannot be read or the entry cannot be
    /// written, and [`Error::Cache`] when a cached entry is corrupt.
    pub fn sha1(&self, path: &Path) -> Result<String> {
        let Some(fingerprint) = Fingerprint::of(path)? else {
            return Err(Error::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )));
        };

        let entry = self.entry_path(path);
        if let Ok(contents) = fs::read_to_string(&entry)
            && let Ok(cached) = serde_json::from_str::<CachedHash>(&contents)
            && cached.algorithm == Algorithm::Sha1.name()
            && cached.fingerprint == fingerprint
        {
            return Ok(cached.value);
        }

        let value = hash_file(path, Algorithm::Sha1)?;
        Self::store(&entry, Algorithm::Sha1, fingerprint, &value)?;

        Ok(value)
    }

    /// Removes a cached digest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when an existing entry cannot be removed.
    pub fn remove(&self, path: &Path) -> Result<()> {
        match fs::remove_file(self.entry_path(path)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Io(error)),
        }
    }

    fn store(
        entry: &Path,
        algorithm: Algorithm,
        fingerprint: Fingerprint,
        value: &str,
    ) -> Result<()> {
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent)?;
        }

        let cached = CachedHash {
            algorithm: algorithm.name().to_owned(),
            fingerprint,
            value: value.to_owned(),
        };
        fs::write(entry, serde_json::to_string(&cached)?)?;

        Ok(())
    }

    fn entry_path(&self, path: &Path) -> PathBuf {
        let digest = blake3::hash(path.to_string_lossy().as_bytes());
        let mut name = String::with_capacity(32);
        for byte in &digest.as_bytes()[..16] {
            let _ = write!(name, "{byte:02x}");
        }

        self.root.join(format!("{name}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_known_values() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("data");
        fs::write(&file, b"hello").unwrap();

        assert_eq!(
            hash_file(&file, Algorithm::Md5).unwrap(),
            "5d41402abc4b2a76b9719d911017c592"
        );
        assert_eq!(
            hash_file(&file, Algorithm::Sha1).unwrap(),
            "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d"
        );
        assert_eq!(
            hash_file(&file, Algorithm::Sha256).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn caches_and_invalidates_sha1() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("data");
        fs::write(&file, b"hello").unwrap();

        let cache = HashCache::new(directory.path().join("cache"));
        let first = cache.sha1(&file).unwrap();
        assert_eq!(first, cache.sha1(&file).unwrap());

        fs::write(&file, b"hello, world").unwrap();
        assert_ne!(first, cache.sha1(&file).unwrap());
    }
}
