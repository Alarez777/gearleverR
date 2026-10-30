//! Detection of running AppImages.

use std::fs;
use std::path::{Path, PathBuf};

/// Returns whether an AppImage is currently running.
///
/// Reads `/proc/<pid>/exe` instead of spawning `ps`, which is faster and does
/// not depend on external tools.
#[must_use]
pub fn is_running(image: &Path) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };

    let target = fs::canonicalize(image).unwrap_or_else(|_| image.to_path_buf());

    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }

        let Ok(link) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };

        let text = link.to_string_lossy();
        let exe = PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text));

        if exe == image || exe == target {
            return true;
        }
    }

    false
}
