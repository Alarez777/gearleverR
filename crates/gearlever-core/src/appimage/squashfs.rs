//! Read-only access to the SquashFS payload of an AppImage.

use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use backhand::{FilesystemReader, InnerNode, Node, SquashfsFileReader};

use crate::error::Result;

/// How many symlinks are followed before giving up.
const MAX_SYMLINK_HOPS: usize = 8;

/// A SquashFS image embedded in an AppImage.
///
/// Opening the image parses its metadata once; file contents are read lazily.
pub struct SquashfsImage {
    reader: FilesystemReader<'static>,
}

impl fmt::Debug for SquashfsImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SquashfsImage")
            .field("entries", &self.reader.files().count())
            .finish_non_exhaustive()
    }
}

impl SquashfsImage {
    /// Opens the SquashFS payload that starts at `offset`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Io`] if the file cannot be opened and
    /// [`crate::Error::Squashfs`] if the payload is not a valid SquashFS image.
    pub fn open(path: &Path, offset: u64) -> Result<Self> {
        let file = File::open(path)?;
        let reader = FilesystemReader::from_reader_with_offset(BufReader::new(file), offset)?;
        Ok(Self { reader })
    }

    /// Iterates over every path in the image, including directories.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.reader.files().map(|node| node.fullpath.as_path())
    }

    /// Returns whether the image contains `path`.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.node(path).is_some()
    }

    /// Reads a file, following symlinks that stay inside the image.
    ///
    /// Returns `Ok(None)` when the path does not exist or is not a regular file
    /// after resolving symlinks.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Squashfs`] if decompression fails and
    /// [`crate::Error::Io`] if the underlying file cannot be read.
    pub fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let mut current = normalize(path);

        for _ in 0..MAX_SYMLINK_HOPS {
            let Some(node) = self.node(&current) else {
                return Ok(None);
            };

            match &node.inner {
                InnerNode::File(file) => {
                    let mut reader = self.reader.file(file).reader();
                    let mut buffer = Vec::new();
                    reader.read_to_end(&mut buffer)?;
                    return Ok(Some(buffer));
                }
                InnerNode::Symlink(symlink) => {
                    current = if symlink.link.is_absolute() {
                        normalize(&symlink.link)
                    } else {
                        let parent = current.parent().unwrap_or_else(|| Path::new(""));
                        normalize(&parent.join(&symlink.link))
                    };
                }
                _ => return Ok(None),
            }
        }

        tracing::warn!(path = %path.display(), "too many symlink hops inside the AppImage");
        Ok(None)
    }

    fn node(&self, path: &Path) -> Option<&Node<SquashfsFileReader>> {
        self.reader.files().find(|node| node.fullpath == path)
    }
}

/// Returns `path` as an absolute path inside the AppImage.
///
/// SquashFS nodes are stored with a leading `/`, so every lookup goes through
/// this normalization; it also resolves `.` and `..` without escaping the root.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");

    for component in path.components() {
        match component {
            std::path::Component::RootDir
            | std::path::Component::Prefix(_)
            | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_absolute_paths() {
        assert_eq!(
            normalize(Path::new("/usr/bin/app")),
            PathBuf::from("/usr/bin/app")
        );
        assert_eq!(
            normalize(Path::new("app.desktop")),
            PathBuf::from("/app.desktop")
        );
        assert_eq!(
            normalize(Path::new("./app.desktop")),
            PathBuf::from("/app.desktop")
        );
        assert_eq!(
            normalize(Path::new("/usr/../app.desktop")),
            PathBuf::from("/app.desktop")
        );
        assert_eq!(
            normalize(Path::new("../../etc/passwd")),
            PathBuf::from("/etc/passwd")
        );
    }
}
