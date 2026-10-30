//! Update checks and downloads.
//!
//! Gear Lever R supports the same six sources as the original: a static URL,
//! GitHub, GitLab, Codeberg, Forgejo and FTP, plus the `.upd_info` section that
//! AppImage developers embed in their bundles.

pub mod embedded;
pub mod ftp;
pub mod gitea;
pub mod github;
pub mod gitlab;
pub mod http;
pub mod static_file;
pub mod zsync;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::appimage::{AppImage, MetadataOptions};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::hash::HashCache;
use crate::install::{Conflict, IntegrateOptions, PreviousInstall, integrate};
use crate::scan::InstalledApp;

pub use http::HttpClient;
pub use zsync::ZsyncInfo;

/// Result of asking an update source whether a newer release exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatus {
    /// A newer release is available.
    Available,
    /// The installed file is current.
    UpToDate,
    /// The source is configured but the release could not be matched.
    Unknown,
}

impl UpdateStatus {
    /// Returns whether an update can be installed.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Kind of update source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerKind {
    /// A direct HTTP(S) URL or a `zsync` URL.
    StaticFile,
    /// GitHub releases.
    Github,
    /// GitLab releases.
    Gitlab,
    /// Codeberg releases.
    Codeberg,
    /// A generic Forgejo instance.
    Forgejo,
    /// An FTP server.
    Ftp,
}

impl ManagerKind {
    /// Every supported source, in the order shown by the original.
    pub const ALL: [Self; 6] = [
        Self::StaticFile,
        Self::Github,
        Self::Gitlab,
        Self::Codeberg,
        Self::Ftp,
        Self::Forgejo,
    ];

    /// Returns the identifier stored in the configuration.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::StaticFile => "StaticFileUpdater",
            Self::Github => "GithubUpdater",
            Self::Gitlab => "GitlabUpdater",
            Self::Codeberg => "CodebergUpdater",
            Self::Forgejo => "ForgejoUpdater",
            Self::Ftp => "FTPUpdater",
        }
    }

    /// Returns the human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::StaticFile => "Static URL",
            Self::Github => "Github",
            Self::Gitlab => "Gitlab",
            Self::Codeberg => "Codeberg",
            Self::Forgejo => "Forgejo",
            Self::Ftp => "FTP",
        }
    }

    /// Returns the `.upd_info` prefix this source understands.
    #[must_use]
    pub const fn embedded_prefix(self) -> Option<&'static str> {
        match self {
            Self::StaticFile => Some("zsync|"),
            Self::Github => Some("gh-releases-zsync|"),
            _ => None,
        }
    }

    /// Parses an identifier from the configuration.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Returns the configuration fields this source needs.
    #[must_use]
    pub fn fields(self) -> Vec<ConfigField> {
        match self {
            Self::StaticFile => vec![ConfigField::text("url", "Update URL")],
            Self::Github | Self::Codeberg => vec![
                ConfigField::text("repo", "Username/Repo"),
                ConfigField::text("repo_filename", "Release file name"),
                ConfigField::boolean("allow_prereleases", "Allow pre-releases"),
            ],
            Self::Gitlab | Self::Forgejo => vec![
                ConfigField::text("repo_url", "Repo URL"),
                ConfigField::text("repo_filename", "Release file name"),
                ConfigField::boolean("allow_prereleases", "Allow pre-releases"),
            ],
            Self::Ftp => vec![
                ConfigField::text("url", "Server URL"),
                ConfigField::text("filename", "File name pattern"),
            ],
        }
    }

    /// Creates the manager for this source.
    #[must_use]
    pub fn create(
        self,
        image: PathBuf,
        config: std::collections::BTreeMap<String, String>,
        embedded: Option<String>,
    ) -> Box<dyn UpdateManager> {
        let http = HttpClient::new();

        match self {
            Self::StaticFile => {
                Box::new(static_file::StaticFile::new(image, config, embedded, http))
            }
            Self::Github => Box::new(github::Github::new(image, config, embedded, http)),
            Self::Gitlab => Box::new(gitlab::Gitlab::new(image, config, embedded, http)),
            Self::Codeberg => Box::new(gitea::Gitea::codeberg(image, config, embedded, http)),
            Self::Forgejo => Box::new(gitea::Gitea::forgejo(image, config, embedded, http)),
            Self::Ftp => Box::new(ftp::Ftp::new(image, config, embedded, http)),
        }
    }
}

impl fmt::Display for ManagerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One configuration field of an update source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigField {
    /// Key stored in the configuration file.
    pub key: &'static str,
    /// Label shown in the interface.
    pub label: &'static str,
    /// Value type.
    pub kind: FieldKind,
}

impl ConfigField {
    const fn text(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Text,
        }
    }

    const fn boolean(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            kind: FieldKind::Boolean,
        }
    }
}

/// Type of a configuration field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Free-form text.
    Text,
    /// Boolean switch.
    Boolean,
}

/// An update source bound to one AppImage.
pub trait UpdateManager: Send + Sync + fmt::Debug {
    /// Returns the source kind.
    fn kind(&self) -> ManagerKind;

    /// Returns the embedded `.upd_info` string, if any.
    fn embedded(&self) -> Option<&str>;

    /// Returns the effective configuration.
    fn config(&self) -> &std::collections::BTreeMap<String, String>;

    /// Checks whether a newer release exists.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] when the network request fails,
    /// [`Error::UpdateConfig`] when required settings are missing, and
    /// [`Error::NoMatchingAsset`] when no release matches the file name.
    fn check(&self, hashes: &HashCache) -> Result<UpdateStatus>;

    /// Downloads the new release into a temporary file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] or [`Error::Io`] when the download fails.
    fn download(&self, context: &mut DownloadContext<'_>) -> Result<PathBuf>;
}

/// Where a download goes and how progress is reported.
pub struct DownloadContext<'a> {
    /// Directory for the temporary download.
    pub directory: PathBuf,
    /// Called with a fraction between 0.0 and 1.0.
    pub progress: &'a mut dyn FnMut(f64),
}

impl fmt::Debug for DownloadContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadContext")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl<'a> DownloadContext<'a> {
    /// Builds a context for `directory`.
    pub fn new(directory: impl Into<PathBuf>, progress: &'a mut dyn FnMut(f64)) -> Self {
        Self {
            directory: directory.into(),
            progress,
        }
    }

    /// Returns a unique path for the downloaded file.
    #[must_use]
    pub fn output_path(&self) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        self.directory
            .join(format!("{}-{nanos}.appimage", std::process::id()))
    }
}

/// Resolves the manager configured for an AppImage, falling back to `.upd_info`.
///
/// # Errors
///
/// Returns [`Error::UpdateConfig`] when the configuration names an unknown
/// manager, and [`Error::InvalidElf`] if the embedded section cannot be read.
pub fn resolve(
    image: &Path,
    config: &std::collections::BTreeMap<String, String>,
) -> Result<Option<Box<dyn UpdateManager>>> {
    if let Some(name) = config.get("manager") {
        let kind = ManagerKind::from_name(name)
            .ok_or_else(|| Error::UpdateConfig(format!("unknown manager {name}")))?;
        return Ok(Some(kind.create(image.to_path_buf(), config.clone(), None)));
    }

    let Some(raw) = embedded::read(image)? else {
        return Ok(None);
    };

    for kind in ManagerKind::ALL {
        if let Some(prefix) = kind.embedded_prefix()
            && raw.starts_with(prefix)
        {
            return Ok(Some(kind.create(
                image.to_path_buf(),
                config.clone(),
                Some(raw),
            )));
        }
    }

    Ok(None)
}

/// Builds a manager for a source selected in the interface.
#[must_use]
pub fn by_name(
    kind: ManagerKind,
    image: &Path,
    config: std::collections::BTreeMap<String, String>,
) -> Box<dyn UpdateManager> {
    kind.create(image.to_path_buf(), config, None)
}

/// Options used when installing a downloaded update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOptions {
    /// Folder that holds AppImages.
    pub appimages_dir: PathBuf,
    /// Folder that holds desktop entries.
    pub applications_dir: PathBuf,
    /// Name terminal apps after their executable.
    pub exec_name_for_terminal_apps: bool,
    /// Run `update-desktop-database` after installing.
    pub update_desktop_database: bool,
    /// Whether an external `--appimage-extract` fallback is allowed.
    pub allow_unsafe_extractor: bool,
}

/// Downloads an update, validates it and replaces the installed AppImage.
///
/// # Errors
///
/// Returns the download, AppImage and integration errors, and
/// [`Error::UpdateConfig`] when the update source is not configured.
pub fn apply(
    app: &InstalledApp,
    manager: &dyn UpdateManager,
    options: &UpdateOptions,
    config: &mut Config,
    download_dir: &Path,
    progress: &mut dyn FnMut(f64),
) -> Result<InstalledApp> {
    let mut context = DownloadContext::new(download_dir.to_path_buf(), progress);
    let downloaded = manager.download(&mut context)?;
    let metadata = AppImage::open(&downloaded)?.metadata_with(&MetadataOptions {
        allow_unsafe_extractor: options.allow_unsafe_extractor,
        work_dir: download_dir.join("metadata"),
    })?;

    let previous = PreviousInstall {
        image_path: app.image_path.clone(),
        desktop_path: app.desktop_path.clone(),
        exec_arguments: app.exec_arguments.clone(),
        env_variables: app.env_variables.clone(),
    };

    let integrate_options = IntegrateOptions {
        appimages_dir: options.appimages_dir.clone(),
        applications_dir: options.applications_dir.clone(),
        conflict: Conflict::Replace,
        move_source: true,
        exec_name_for_terminal_apps: options.exec_name_for_terminal_apps,
        name_suffix: None,
        update_desktop_database: options.update_desktop_database,
    };

    let installed = integrate(
        &downloaded,
        &metadata,
        Some(&previous),
        &integrate_options,
        config,
    )?;

    if downloaded.exists() {
        let _ = std::fs::remove_file(&downloaded);
    }

    Ok(installed)
}

/// A release asset offered by a forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asset {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) size: Option<u64>,
    pub(crate) digest: Option<String>,
}

/// Returns the first asset matching `pattern`, preferring this architecture.
pub(crate) fn pick_asset(assets: &[Asset], pattern: &str) -> Option<Asset> {
    let matcher = glob::Pattern::new(pattern).ok()?;
    let candidates: Vec<&Asset> = assets
        .iter()
        .filter(|asset| matcher.matches(&asset.name))
        .collect();

    if candidates.len() == 1 {
        return candidates.first().map(|asset| (*asset).clone());
    }

    let is_arm = |name: &str| {
        ["arm64", "aarch64", "armv7l"]
            .iter()
            .any(|token| name.contains(token))
    };
    let is_x86 =
        |name: &str| name.contains("x86_64") || name.contains("x86-64") || name.contains("amd64");

    if cfg!(target_arch = "x86_64") {
        candidates
            .iter()
            .find(|asset| is_x86(&asset.name) || !is_arm(&asset.name))
            .map(|asset| (*asset).clone())
    } else {
        candidates
            .iter()
            .find(|asset| is_arm(&asset.name) || !is_x86(&asset.name))
            .map(|asset| (*asset).clone())
    }
}

/// Returns the configured value, trimmed.
pub(crate) fn value<'a>(
    config: &'a std::collections::BTreeMap<String, String>,
    key: &str,
) -> Option<&'a str> {
    config
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

/// Returns a configured boolean.
pub(crate) fn boolean(config: &std::collections::BTreeMap<String, String>, key: &str) -> bool {
    config.get(key).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "yes" | "1" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_owned(),
            url: format!("https://example.com/{name}"),
            size: Some(10),
            digest: None,
        }
    }

    #[test]
    fn picks_the_only_matching_asset() {
        let assets = vec![asset("app-x86_64.AppImage"), asset("app-aarch64.AppImage")];

        assert_eq!(
            pick_asset(&assets, "app-*.AppImage").map(|asset| asset.name),
            Some("app-x86_64.AppImage".to_owned())
        );
    }

    #[test]
    fn manager_names_match_the_original() {
        assert_eq!(ManagerKind::Github.name(), "GithubUpdater");
        assert_eq!(ManagerKind::from_name("FTPUpdater"), Some(ManagerKind::Ftp));
        assert_eq!(ManagerKind::from_name("nope"), None);
    }
}
