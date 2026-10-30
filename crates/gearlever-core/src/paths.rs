//! XDG base directories and the files Gear Lever R keeps in them.

use std::env;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Filesystem locations used by Gear Lever R.
///
/// Construction is explicit: [`Paths::from_env`] reads `HOME` and the XDG
/// variables, while [`Paths::with_home`] builds the default layout below a
/// directory and is meant for tests and sandboxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
}

impl Paths {
    /// Resolves the XDG directories from the environment.
    ///
    /// Relative XDG values are ignored, as required by the XDG Base Directory
    /// Specification, and fall back to the matching directory below `HOME`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoHomeDirectory`] when `HOME` is unset or empty.
    pub fn from_env() -> Result<Self> {
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(Error::NoHomeDirectory)?;

        Ok(Self {
            config_dir: xdg_dir("XDG_CONFIG_HOME", &home, ".config"),
            data_dir: xdg_dir("XDG_DATA_HOME", &home, ".local/share"),
            cache_dir: xdg_dir("XDG_CACHE_HOME", &home, ".cache"),
            state_dir: xdg_dir("XDG_STATE_HOME", &home, ".local/state"),
            home,
        })
    }

    /// Builds the default layout below `home`, without reading the environment.
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();

        Self {
            config_dir: home.join(".config"),
            data_dir: home.join(".local/share"),
            cache_dir: home.join(".cache"),
            state_dir: home.join(".local/state"),
            home,
        }
    }

    /// Returns the user's home directory.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Returns `$XDG_CONFIG_HOME`.
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Returns `$XDG_DATA_HOME`.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Returns `$XDG_CACHE_HOME`.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Returns `$XDG_STATE_HOME`.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns Gear Lever R's own configuration file.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("gearlever-r.conf")
    }

    /// Returns the configuration file written by the original Gear Lever.
    #[must_use]
    pub fn legacy_config_file(&self) -> PathBuf {
        self.config_dir.join("gearlever.conf")
    }

    /// Returns the directory that holds integrated desktop entries.
    #[must_use]
    pub fn applications_dir(&self) -> PathBuf {
        self.data_dir.join("applications")
    }

    /// Returns the default folder where AppImages are kept.
    #[must_use]
    pub fn default_appimages_dir(&self) -> PathBuf {
        self.home.join("AppImages")
    }

    /// Returns the hidden icons folder inside `appimages_dir`.
    #[must_use]
    pub fn icons_dir(&self, appimages_dir: &Path) -> PathBuf {
        appimages_dir.join(".icons")
    }

    /// Returns the root of Gear Lever R's cache.
    #[must_use]
    pub fn cache_root(&self) -> PathBuf {
        self.cache_dir.join("gearlever-r")
    }

    /// Returns the metadata cache directory.
    #[must_use]
    pub fn metadata_cache_dir(&self) -> PathBuf {
        self.cache_root().join("metadata")
    }

    /// Returns the directory for logs and other state.
    #[must_use]
    pub fn state_root(&self) -> PathBuf {
        self.state_dir.join("gearlever-r")
    }

    /// Returns the log file path.
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.state_root().join("gearlever-r.log")
    }

    /// Returns a per-process temporary directory.
    #[must_use]
    pub fn temp_dir(&self) -> PathBuf {
        env::temp_dir().join(format!("gearlever-r-{}", std::process::id()))
    }

    /// Returns the autostart entry used for background update checks.
    #[must_use]
    pub fn autostart_file(&self) -> PathBuf {
        self.config_dir
            .join("autostart")
            .join("io.github.alarez777.GearLeverR.desktop")
    }
}

/// Resolves one XDG variable, ignoring relative values.
fn xdg_dir(variable: &str, home: &Path, fallback: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .filter(|value| value.is_absolute())
        .unwrap_or_else(|| home.join(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_lives_below_home() {
        let paths = Paths::with_home("/home/tester");

        assert_eq!(
            paths.config_file(),
            PathBuf::from("/home/tester/.config/gearlever-r.conf")
        );
        assert_eq!(
            paths.legacy_config_file(),
            PathBuf::from("/home/tester/.config/gearlever.conf")
        );
        assert_eq!(
            paths.applications_dir(),
            PathBuf::from("/home/tester/.local/share/applications")
        );
        assert_eq!(
            paths.default_appimages_dir(),
            PathBuf::from("/home/tester/AppImages")
        );
        assert_eq!(
            paths.icons_dir(Path::new("/opt/apps")),
            PathBuf::from("/opt/apps/.icons")
        );
        assert_eq!(
            paths.metadata_cache_dir(),
            PathBuf::from("/home/tester/.cache/gearlever-r/metadata")
        );
    }
}
