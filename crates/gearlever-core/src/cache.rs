//! Persistent cache for metadata extracted from AppImages.
//!
//! Extracting an icon means reading the SquashFS payload, which is cheap but not
//! free. Entries are keyed by path and invalidated by size and modification
//! time, so a replaced AppImage is extracted again automatically.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::appimage::AppMetadata;
use crate::error::{Error, Result};

/// Metadata of an AppImage that was read from the cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedMetadata {
    /// `Name` of the embedded desktop entry.
    pub name: String,
    /// `Comment` of the embedded desktop entry.
    pub comment: String,
    /// `X-AppImage-Version`, when declared.
    pub version: Option<String>,
    /// `Exec` line as shipped.
    pub exec: String,
    /// `TryExec` line as shipped.
    pub try_exec: String,
    /// Whether the application runs in a terminal.
    pub terminal: bool,
    /// File name of the cached icon, relative to the entry directory.
    pub icon_file: Option<String>,
}

/// A persistent metadata cache rooted at a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataCache {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    metadata: CachedMetadata,
    size: u64,
    modified_seconds: u64,
    modified_nanos: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    size: u64,
    modified_seconds: u64,
    modified_nanos: u32,
}

impl MetadataCache {
    /// Creates a cache rooted at `root`, which is created on first write.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the cached metadata for `image` when it is still valid.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the entry exists but cannot be read and
    /// [`Error::Cache`] when it contains invalid JSON.
    pub fn load(&self, image: &Path) -> Result<Option<CachedMetadata>> {
        let Some(fingerprint) = fingerprint(image)? else {
            return Ok(None);
        };

        let entry_path = self.entry_dir(image).join("meta.json");
        let contents = match fs::read_to_string(&entry_path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::Io(error)),
        };

        let entry: Entry = serde_json::from_str(&contents)?;
        if entry.size != fingerprint.size
            || entry.modified_seconds != fingerprint.modified_seconds
            || entry.modified_nanos != fingerprint.modified_nanos
        {
            return Ok(None);
        }

        Ok(Some(entry.metadata))
    }

    /// Stores `metadata` for `image`, replacing any previous entry.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the entry cannot be written and
    /// [`Error::Cache`] when it cannot be serialized.
    pub fn store(&self, image: &Path, metadata: &AppMetadata) -> Result<CachedMetadata> {
        let fingerprint = fingerprint(image)?
            .ok_or_else(|| Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound)))?;
        let directory = self.entry_dir(image);

        if directory.exists() {
            fs::remove_dir_all(&directory)?;
        }
        fs::create_dir_all(&directory)?;

        let icon_file = match &metadata.icon {
            Some(icon) => {
                let extension = Path::new(&icon.file_name)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("png");
                let file_name = format!("icon.{extension}");
                fs::write(directory.join(&file_name), &icon.bytes)?;
                Some(file_name)
            }
            None => None,
        };

        let cached = CachedMetadata {
            name: metadata.name.clone(),
            comment: metadata.comment.clone(),
            version: metadata.version.clone(),
            exec: metadata.exec.clone(),
            try_exec: metadata.try_exec.clone(),
            terminal: metadata.terminal,
            icon_file,
        };

        let entry = Entry {
            metadata: cached.clone(),
            size: fingerprint.size,
            modified_seconds: fingerprint.modified_seconds,
            modified_nanos: fingerprint.modified_nanos,
        };

        let json = serde_json::to_string_pretty(&entry)?;
        let temporary = directory.join("meta.json.tmp");
        fs::write(&temporary, json)?;
        fs::rename(temporary, directory.join("meta.json"))?;

        Ok(cached)
    }

    /// Returns the absolute path of the cached icon, if any.
    #[must_use]
    pub fn icon_path(&self, image: &Path, metadata: &CachedMetadata) -> Option<PathBuf> {
        metadata
            .icon_file
            .as_ref()
            .map(|file_name| self.entry_dir(image).join(file_name))
    }

    fn entry_dir(&self, image: &Path) -> PathBuf {
        self.root.join(hash_path(image))
    }
}

fn hash_path(image: &Path) -> String {
    let digest = blake3::hash(image.to_string_lossy().as_bytes());
    let mut hex = String::with_capacity(32);

    for byte in &digest.as_bytes()[..16] {
        let _ = write!(hex, "{byte:02x}");
    }

    hex
}

fn fingerprint(image: &Path) -> Result<Option<Fingerprint>> {
    let metadata = match fs::metadata(image) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Io(error)),
    };

    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let elapsed = modified.duration_since(UNIX_EPOCH).unwrap_or_default();

    Ok(Some(Fingerprint {
        size: metadata.len(),
        modified_seconds: elapsed.as_secs(),
        modified_nanos: elapsed.subsec_nanos(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appimage::ExtractedIcon;

    fn metadata() -> AppMetadata {
        AppMetadata {
            name: "Cached".to_owned(),
            comment: "From cache".to_owned(),
            exec: "AppRun".to_owned(),
            try_exec: "AppRun".to_owned(),
            icon_name: Some("cached".to_owned()),
            terminal: false,
            version: Some("1.0".to_owned()),
            desktop_file: Some("/cached.desktop".to_owned()),
            desktop_contents: "[Desktop Entry]\nName=Cached\n".to_owned(),
            icon: Some(ExtractedIcon {
                file_name: "cached.png".to_owned(),
                bytes: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            }),
        }
    }

    #[test]
    fn stores_loads_and_invalidates_entries() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("cached.appimage");
        fs::write(&image, b"first").unwrap();

        let cache = MetadataCache::new(directory.path().join("cache"));
        assert_eq!(cache.load(&image).unwrap(), None);

        let cached = cache.store(&image, &metadata()).unwrap();
        assert_eq!(cached.name, "Cached");
        assert_eq!(cache.load(&image).unwrap(), Some(cached.clone()));
        assert!(cache.icon_path(&image, &cached).unwrap().exists());

        fs::write(&image, b"second version, larger").unwrap();
        assert_eq!(cache.load(&image).unwrap(), None, "size change invalidates");
    }

    #[test]
    fn missing_image_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let cache = MetadataCache::new(directory.path());

        assert_eq!(
            cache.load(&directory.path().join("gone.appimage")).unwrap(),
            None
        );
    }
}
