//! Update checks and downloads.
//!
//! Gear LeverR supports the same six sources as the original: a static URL,
//! GitHub, GitLab, Codeberg, Forgejo and FTP, plus the `.upd_info` section that
//! AppImage developers embed in their bundles.

pub mod embedded;
pub mod ftp;
pub mod gitea;
pub mod github;
pub mod gitlab;
pub mod http;
mod http_cache;
pub mod static_file;
pub mod zsync;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::appimage::{AppImage, AppMetadata, MetadataOptions};
use crate::config::Config;
use crate::error::{Error, RateLimitInfo, Result};
use crate::hash::HashCache;
use crate::install::{Conflict, IntegrateOptions, PreviousInstall, integrate};
use crate::scan::InstalledApp;

pub use http::HttpClient;
pub use zsync::ZsyncInfo;

/// Maximum update checks in flight at once.
pub const DEFAULT_PARALLEL_CHECKS: usize = 5;

/// Maximum time spent retrying failed update checks.
pub const DEFAULT_RETRY_BUDGET: Duration = Duration::from_secs(20);

/// Maximum updates prepared at once during a batch.
pub const DEFAULT_PARALLEL_DOWNLOADS: usize = 3;

/// A lazily resolved update target shared by an [`UpdateManager`].
///
/// The cache is cleared when the manager is cloned, because a clone is a new
/// attempt and must not reuse a target resolved by another manager.
#[derive(Debug)]
struct TargetCache<T> {
    value: Mutex<Option<T>>,
}

impl<T> TargetCache<T> {
    const fn new() -> Self {
        Self {
            value: Mutex::new(None),
        }
    }

    fn set(&self, value: T) {
        *self
            .value
            .lock()
            .expect("update target cache lock poisoned") = Some(value);
    }
}

impl<T: Clone> TargetCache<T> {
    fn get(&self) -> Option<T> {
        self.value
            .lock()
            .expect("update target cache lock poisoned")
            .clone()
    }
}

impl<T> Clone for TargetCache<T> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

/// Result of asking an update source whether a newer release exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
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
        token: Option<&str>,
    ) -> Box<dyn UpdateManager> {
        let http = HttpClient::shared();

        match self {
            Self::StaticFile => {
                Box::new(static_file::StaticFile::new(image, config, embedded, http))
            }
            Self::Github => Box::new(github::Github::new(image, config, embedded, http, token)),
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
#[non_exhaustive]
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
    /// Set by the interface when the user cancels the download.
    pub cancel: Arc<AtomicBool>,
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
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Builds a context that shares a cancellation flag with the caller.
    #[must_use]
    pub fn with_cancel(
        directory: impl Into<PathBuf>,
        progress: &'a mut dyn FnMut(f64),
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            directory: directory.into(),
            progress,
            cancel,
        }
    }

    /// Returns whether the user asked to cancel.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
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
    github_token: Option<&str>,
) -> Result<Option<Box<dyn UpdateManager>>> {
    if let Some(name) = config.get("manager") {
        let kind = ManagerKind::from_name(name)
            .ok_or_else(|| Error::UpdateConfig(format!("unknown manager {name}")))?;
        return Ok(Some(kind.create(
            image.to_path_buf(),
            config.clone(),
            None,
            github_token,
        )));
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
                github_token,
            )));
        }
    }

    Ok(None)
}

/// Validates the configuration of an update source before saving it.
///
/// # Errors
///
/// Returns [`Error::UpdateConfig`] with a human-readable reason when a required
/// field is missing or malformed.
pub fn validate_config(
    kind: ManagerKind,
    values: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let get = |key: &str| values.get(key).map(String::as_str).unwrap_or_default();

    match kind {
        ManagerKind::StaticFile => {
            let url = get("url");
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(Error::UpdateConfig("enter a valid HTTP URL".to_owned()));
            }
        }
        ManagerKind::Ftp => {
            if !get("url").starts_with("ftp://") {
                return Err(Error::UpdateConfig("enter a valid FTP URL".to_owned()));
            }
            if get("filename").is_empty() {
                return Err(Error::UpdateConfig("enter a file name pattern".to_owned()));
            }
        }
        ManagerKind::Github | ManagerKind::Codeberg => {
            if get("repo").split('/').count() != 2 {
                return Err(Error::UpdateConfig("enter <username>/<repo>".to_owned()));
            }
            if get("repo_filename").is_empty() {
                return Err(Error::UpdateConfig("enter a release file name".to_owned()));
            }
        }
        ManagerKind::Gitlab | ManagerKind::Forgejo => {
            if !get("repo_url").starts_with("https://") {
                return Err(Error::UpdateConfig(
                    "enter a valid repository URL".to_owned(),
                ));
            }
            if get("repo_filename").is_empty() {
                return Err(Error::UpdateConfig("enter a release file name".to_owned()));
            }
        }
    }

    Ok(())
}

/// Builds a manager for a source selected in the interface.
#[must_use]
pub fn by_name(
    kind: ManagerKind,
    image: &Path,
    config: std::collections::BTreeMap<String, String>,
    github_token: Option<&str>,
) -> Box<dyn UpdateManager> {
    kind.create(image.to_path_buf(), config, None, github_token)
}

/// An installed AppImage paired with the source that updates it.
#[derive(Debug, Clone)]
pub struct UpdateJob {
    /// Application to update.
    pub app: InstalledApp,
    /// Source that resolves and downloads the new release.
    pub manager: Arc<dyn UpdateManager>,
}

impl UpdateJob {
    /// Pairs an application with its update source.
    #[must_use]
    pub fn new(app: InstalledApp, manager: Arc<dyn UpdateManager>) -> Self {
        Self { app, manager }
    }
}

/// Event emitted while checking a batch of update sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckEvent {
    /// One check finished.
    Checked {
        /// Position of the job in the input slice.
        index: usize,
        /// Result reported by the source.
        status: UpdateStatus,
    },
    /// The first pass finished with transient failures and retries are starting.
    Retrying,
    /// The configured GitHub token was rejected.
    TokenInvalid,
    /// GitHub exhausted the API rate limit.
    RateLimited(RateLimitInfo),
}

/// Result of checking a batch of update sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Final status of every job, in the same order as the input slice.
    pub outcomes: Vec<UpdateStatus>,
    /// Whether GitHub rejected the configured token.
    pub token_invalid: bool,
    /// Rate-limit details when GitHub exhausted the API limit.
    pub rate_limit: Option<RateLimitInfo>,
}

/// Event emitted while installing a batch of updates.
#[derive(Debug)]
#[non_exhaustive]
pub enum BatchEvent {
    /// Overall progress between 0.0 and 1.0.
    Progress {
        /// Position of the job in the input slice.
        index: usize,
        /// Fraction of the whole batch that has finished preparing or installing.
        overall: f64,
    },
    /// One update was installed successfully.
    Installed {
        /// Position of the job in the input slice.
        index: usize,
        /// Updated application.
        app: InstalledApp,
    },
    /// One update failed.
    Failed {
        /// Position of the job in the input slice.
        index: usize,
        /// Application that could not be updated.
        app: InstalledApp,
        /// Failure reported by the download or integration step.
        error: Error,
    },
    /// One update was cancelled before it could be installed.
    Cancelled {
        /// Position of the job in the input slice.
        index: usize,
    },
}

/// Summary of a batch update.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct BatchReport {
    /// Applications that were updated successfully.
    pub installed: Vec<InstalledApp>,
    /// AppImages that could not be updated.
    pub failed: Vec<PathBuf>,
    /// Whether the caller cancelled the batch.
    pub cancelled: bool,
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

impl UpdateOptions {
    /// Returns the staging directory used for downloads inside the AppImages
    /// folder, so the final install is an atomic rename on the same filesystem.
    #[must_use]
    pub fn staging_dir(&self) -> PathBuf {
        self.appimages_dir.join(".staging")
    }
}

/// A downloaded and validated update waiting to be integrated.
struct PreparedUpdate {
    app: InstalledApp,
    metadata: AppMetadata,
    downloaded: PathBuf,
}

/// Downloads an update and reads its metadata without touching the installed app.
fn prepare(
    app: &InstalledApp,
    manager: &dyn UpdateManager,
    options: &UpdateOptions,
    download_dir: &Path,
    progress: &mut dyn FnMut(f64),
    cancel: Arc<AtomicBool>,
) -> Result<PreparedUpdate> {
    std::fs::create_dir_all(download_dir)?;
    let mut context = DownloadContext::with_cancel(download_dir.to_path_buf(), progress, cancel);
    let downloaded = manager.download(&mut context)?;

    if context.is_cancelled() {
        let _ = std::fs::remove_file(&downloaded);
        return Err(Error::Cancelled);
    }

    let metadata_options = MetadataOptions {
        allow_unsafe_extractor: options.allow_unsafe_extractor,
        work_dir: download_dir.join("metadata"),
    };
    let metadata = match AppImage::open(&downloaded)
        .and_then(|image| image.metadata_with(&metadata_options))
    {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = std::fs::remove_file(&downloaded);
            return Err(error);
        }
    };

    Ok(PreparedUpdate {
        app: app.clone(),
        metadata,
        downloaded,
    })
}

/// Integrates a prepared update, replacing the installed AppImage.
fn commit(
    prepared: &PreparedUpdate,
    options: &UpdateOptions,
    config: &mut Config,
) -> Result<InstalledApp> {
    let previous = PreviousInstall {
        image_path: prepared.app.image_path.clone(),
        desktop_path: prepared.app.desktop_path.clone(),
        exec_arguments: prepared.app.exec_arguments.clone(),
        env_variables: prepared.app.env_variables.clone(),
    };
    let destination = crate::install::replacement_destination(&previous, &options.appimages_dir)?;

    if let Err(error) = crate::install::stage_download(&prepared.downloaded, &destination) {
        let _ = std::fs::remove_file(&prepared.downloaded);
        return Err(error);
    }

    let temporary = crate::install::temporary_path(&destination);
    let integrate_options = IntegrateOptions {
        appimages_dir: options.appimages_dir.clone(),
        applications_dir: options.applications_dir.clone(),
        conflict: Conflict::Replace,
        move_source: true,
        exec_name_for_terminal_apps: options.exec_name_for_terminal_apps,
        name_suffix: None,
        update_desktop_database: options.update_desktop_database,
    };

    let result = integrate(
        &temporary,
        &prepared.metadata,
        Some(&previous),
        &integrate_options,
        config,
    );
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }

    result
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
    cancel: Arc<AtomicBool>,
) -> Result<InstalledApp> {
    let prepared = prepare(app, manager, options, download_dir, progress, cancel)?;
    commit(&prepared, options, config)
}

/// Checks a batch of update sources with a bounded worker pool.
///
/// Transient failures are retried once after the first pass, with a shared
/// deadline. Events are emitted on the calling thread, so callers can update
/// their interface without synchronising their own state.
///
/// # Panics
///
/// Panics if an internal worker panics or a lock is poisoned.
#[expect(
    clippy::too_many_lines,
    reason = "the parallel check and retry policy share one worker pool"
)]
pub fn check_batch(
    jobs: &[UpdateJob],
    hashes: &HashCache,
    workers: usize,
    retry_budget: Duration,
    on_event: &mut dyn FnMut(CheckEvent),
) -> CheckReport {
    let total = jobs.len();
    let mut statuses: Vec<Option<UpdateStatus>> = vec![None; total];
    let token_invalid = AtomicBool::new(false);
    let rate_limited = AtomicBool::new(false);
    let rate_limit = Mutex::new(None);
    let failures = Mutex::new(Vec::<(usize, Duration)>::new());

    if total > 0 {
        let worker_count = workers.max(1).min(total);
        let next = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            let (sender, receiver) = mpsc::channel::<CheckWorker>();

            for _ in 0..worker_count {
                let sender = sender.clone();
                let next = &next;
                let token_invalid = &token_invalid;
                let rate_limited = &rate_limited;
                let rate_limit = &rate_limit;
                let failures = &failures;

                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.get(index) else {
                            break;
                        };

                        match job.manager.check(hashes) {
                            Ok(status) => {
                                let _ = sender.send(CheckWorker::Checked { index, status });
                            }
                            Err(error @ Error::GithubTokenInvalid) => {
                                if !token_invalid.swap(true, Ordering::Relaxed) {
                                    let _ = sender.send(CheckWorker::TokenInvalid);
                                }
                                tracing::warn!(
                                    app = %job.app.name,
                                    %error,
                                    "update check failed"
                                );
                                let _ = sender.send(CheckWorker::Checked {
                                    index,
                                    status: UpdateStatus::Unknown,
                                });
                            }
                            Err(error) => {
                                if let Some(delay) = error.retry_delay() {
                                    tracing::debug!(
                                        app = %job.app.name,
                                        %error,
                                        "update check failed, will retry"
                                    );
                                    failures
                                        .lock()
                                        .expect("update-check retry queue lock poisoned")
                                        .push((index, delay));
                                } else {
                                    if let Some(info) = error.rate_limit_info() {
                                        let mut slot = rate_limit
                                            .lock()
                                            .expect("update-check rate limit lock poisoned");
                                        if slot.is_none() {
                                            *slot = Some(info);
                                        }
                                        drop(slot);

                                        if !rate_limited.swap(true, Ordering::Relaxed) {
                                            let _ = sender.send(CheckWorker::RateLimited(info));
                                        }
                                        tracing::warn!(
                                            app = %job.app.name,
                                            %error,
                                            "GitHub API rate limit exhausted"
                                        );
                                    } else {
                                        tracing::warn!(
                                            app = %job.app.name,
                                            %error,
                                            "update check failed"
                                        );
                                    }
                                    let _ = sender.send(CheckWorker::Checked {
                                        index,
                                        status: UpdateStatus::Unknown,
                                    });
                                }
                            }
                        }
                    }
                });
            }

            drop(sender);

            for message in receiver {
                match message {
                    CheckWorker::Checked { index, status } => {
                        statuses[index] = Some(status);
                        on_event(CheckEvent::Checked { index, status });
                    }
                    CheckWorker::TokenInvalid => on_event(CheckEvent::TokenInvalid),
                    CheckWorker::RateLimited(info) => on_event(CheckEvent::RateLimited(info)),
                }
            }
        });
    }

    let mut failures = failures
        .into_inner()
        .expect("update-check retry queue lock poisoned");
    failures.sort_unstable_by_key(|(index, _)| *index);

    if !failures.is_empty() {
        on_event(CheckEvent::Retrying);
        let deadline = Instant::now() + retry_budget;

        for (index, delay) in failures {
            let job = &jobs[index];
            let now = Instant::now();
            if now >= deadline || delay > deadline.saturating_duration_since(now) {
                tracing::warn!(
                    app = %job.app.name,
                    "update check not retried: retry budget exhausted"
                );
                statuses[index] = Some(UpdateStatus::Unknown);
                on_event(CheckEvent::Checked {
                    index,
                    status: UpdateStatus::Unknown,
                });
                continue;
            }

            if !delay.is_zero() {
                std::thread::sleep(delay);
            }

            match job.manager.check(hashes) {
                Ok(status) => {
                    tracing::info!(
                        app = %job.app.name,
                        "update check succeeded after retry"
                    );
                    statuses[index] = Some(status);
                    on_event(CheckEvent::Checked { index, status });
                }
                Err(error @ Error::GithubTokenInvalid) => {
                    if !token_invalid.swap(true, Ordering::Relaxed) {
                        on_event(CheckEvent::TokenInvalid);
                    }
                    tracing::warn!(
                        app = %job.app.name,
                        %error,
                        "update check failed after retry"
                    );
                    statuses[index] = Some(UpdateStatus::Unknown);
                    on_event(CheckEvent::Checked {
                        index,
                        status: UpdateStatus::Unknown,
                    });
                }
                Err(error) => {
                    if let Some(info) = error.rate_limit_info() {
                        let mut slot = rate_limit
                            .lock()
                            .expect("update-check rate limit lock poisoned");
                        if slot.is_none() {
                            *slot = Some(info);
                        }
                        drop(slot);

                        if !rate_limited.swap(true, Ordering::Relaxed) {
                            on_event(CheckEvent::RateLimited(info));
                        }
                        tracing::warn!(
                            app = %job.app.name,
                            %error,
                            "GitHub API rate limit exhausted after retry"
                        );
                    } else {
                        tracing::warn!(
                            app = %job.app.name,
                            %error,
                            "update check failed after retry"
                        );
                    }
                    statuses[index] = Some(UpdateStatus::Unknown);
                    on_event(CheckEvent::Checked {
                        index,
                        status: UpdateStatus::Unknown,
                    });
                }
            }
        }
    }

    CheckReport {
        outcomes: statuses
            .into_iter()
            .map(|status| status.unwrap_or(UpdateStatus::Unknown))
            .collect(),
        token_invalid: token_invalid.load(Ordering::Relaxed),
        rate_limit: rate_limit
            .into_inner()
            .expect("update-check rate limit lock poisoned"),
    }
}

/// Message sent by a check worker to the coordinator.
enum CheckWorker {
    Checked { index: usize, status: UpdateStatus },
    TokenInvalid,
    RateLimited(RateLimitInfo),
}

/// Message sent by a download worker to the coordinator.
enum WorkerUpdate {
    Progress {
        index: usize,
        fraction: f64,
    },
    Prepared {
        index: usize,
        update: PreparedUpdate,
    },
    Failed {
        index: usize,
        app: InstalledApp,
        error: Error,
    },
    Cancelled {
        index: usize,
    },
}

/// Returns the average progress of every job in a batch.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "update progress does not need exact precision"
)]
fn batch_progress(fractions: &[f64]) -> f64 {
    fractions.iter().sum::<f64>() / fractions.len() as f64
}

/// Emits the aggregate progress of a batch.
fn emit_batch_progress(fractions: &[f64], index: usize, on_event: &mut dyn FnMut(BatchEvent)) {
    on_event(BatchEvent::Progress {
        index,
        overall: batch_progress(fractions),
    });
}

/// Downloads and installs a batch of updates.
///
/// Downloads and metadata extraction run in parallel, while integration stays
/// serial so `Config`, desktop entries and icons are never written concurrently.
/// `update-desktop-database` runs once at the end when the option is enabled.
#[expect(
    clippy::too_many_lines,
    reason = "the worker pool, coordinator and progress bookkeeping share one pipeline"
)]
pub fn apply_batch(
    jobs: &[UpdateJob],
    options: &UpdateOptions,
    config: &mut Config,
    download_dir: &Path,
    parallel: usize,
    cancel: &Arc<AtomicBool>,
    on_event: &mut dyn FnMut(BatchEvent),
) -> BatchReport {
    let total = jobs.len();
    if total == 0 {
        return BatchReport::default();
    }

    let worker_count = parallel.max(1).min(total);
    let mut fractions = vec![0.0_f64; total];
    let mut installed = Vec::new();
    let mut failed = Vec::new();
    let mut cancelled = false;

    let mut batch_options = options.clone();
    batch_options.update_desktop_database = false;

    let next = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel::<WorkerUpdate>();

        for _ in 0..worker_count {
            let sender = sender.clone();
            let options = &batch_options;
            let next = &next;

            scope.spawn(move || {
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        break;
                    };

                    let progress_sender = sender.clone();
                    let mut progress = |fraction: f64| {
                        let _ = progress_sender.send(WorkerUpdate::Progress { index, fraction });
                    };

                    match prepare(
                        &job.app,
                        job.manager.as_ref(),
                        options,
                        download_dir,
                        &mut progress,
                        Arc::clone(cancel),
                    ) {
                        Ok(update) => {
                            let _ = sender.send(WorkerUpdate::Prepared { index, update });
                        }
                        Err(Error::Cancelled) => {
                            let _ = sender.send(WorkerUpdate::Cancelled { index });
                        }
                        Err(error) => {
                            let _ = sender.send(WorkerUpdate::Failed {
                                index,
                                app: job.app.clone(),
                                error,
                            });
                        }
                    }
                }
            });
        }

        drop(sender);

        for message in receiver {
            match message {
                WorkerUpdate::Progress { index, fraction } => {
                    fractions[index] = fraction.clamp(0.0, 1.0);
                    emit_batch_progress(&fractions, index, on_event);
                }
                WorkerUpdate::Prepared { index, update } => {
                    if cancel.load(Ordering::Relaxed) {
                        let _ = std::fs::remove_file(&update.downloaded);
                        fractions[index] = 1.0;
                        cancelled = true;
                        on_event(BatchEvent::Cancelled { index });
                        emit_batch_progress(&fractions, index, on_event);
                        continue;
                    }

                    let app = update.app.clone();
                    match commit(&update, &batch_options, config) {
                        Ok(installed_app) => {
                            fractions[index] = 1.0;
                            installed.push(installed_app.clone());
                            on_event(BatchEvent::Installed {
                                index,
                                app: installed_app,
                            });
                        }
                        Err(error) => {
                            fractions[index] = 1.0;
                            failed.push(app.image_path.clone());
                            on_event(BatchEvent::Failed { index, app, error });
                        }
                    }
                    emit_batch_progress(&fractions, index, on_event);
                }
                WorkerUpdate::Failed { index, app, error } => {
                    fractions[index] = 1.0;
                    failed.push(app.image_path.clone());
                    on_event(BatchEvent::Failed { index, app, error });
                    emit_batch_progress(&fractions, index, on_event);
                }
                WorkerUpdate::Cancelled { index } => {
                    fractions[index] = 1.0;
                    cancelled = true;
                    on_event(BatchEvent::Cancelled { index });
                    emit_batch_progress(&fractions, index, on_event);
                }
            }
        }
    });

    if cancelled {
        tracing::debug!("update batch cancelled");
    }

    if options.update_desktop_database && !installed.is_empty() {
        crate::install::refresh_desktop_database(&options.applications_dir);
    }

    BatchReport {
        installed,
        failed,
        cancelled: cancelled || cancel.load(Ordering::Relaxed),
    }
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
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::error::HttpStatus;

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
    fn validates_update_configuration() {
        let valid = std::collections::BTreeMap::from([
            ("repo".to_owned(), "owner/repo".to_owned()),
            ("repo_filename".to_owned(), "App.AppImage".to_owned()),
        ]);
        assert!(validate_config(ManagerKind::Github, &valid).is_ok());

        let invalid = std::collections::BTreeMap::from([
            ("repo".to_owned(), "owner".to_owned()),
            ("repo_filename".to_owned(), "App.AppImage".to_owned()),
        ]);
        assert!(validate_config(ManagerKind::Github, &invalid).is_err());

        let static_url =
            std::collections::BTreeMap::from([("url".to_owned(), "not-a-url".to_owned())]);
        assert!(validate_config(ManagerKind::StaticFile, &static_url).is_err());
    }

    #[test]
    fn manager_names_match_the_original() {
        assert_eq!(ManagerKind::Github.name(), "GithubUpdater");
        assert_eq!(ManagerKind::from_name("FTPUpdater"), Some(ManagerKind::Ftp));
        assert_eq!(ManagerKind::from_name("nope"), None);
    }

    #[test]
    fn target_cache_keeps_the_resolved_value() {
        let cache = TargetCache::new();
        assert_eq!(cache.get(), None);

        cache.set(7_u32);
        assert_eq!(cache.get(), Some(7));
    }

    #[derive(Debug)]
    struct FakeManager {
        config: BTreeMap<String, String>,
        statuses: Mutex<VecDeque<Result<UpdateStatus>>>,
        delay: Duration,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    impl FakeManager {
        fn new(statuses: impl IntoIterator<Item = Result<UpdateStatus>>) -> Self {
            Self {
                config: BTreeMap::new(),
                statuses: Mutex::new(statuses.into_iter().collect()),
                delay: Duration::ZERO,
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }
    }

    impl UpdateManager for FakeManager {
        fn kind(&self) -> ManagerKind {
            ManagerKind::StaticFile
        }

        fn embedded(&self) -> Option<&str> {
            None
        }

        fn config(&self) -> &BTreeMap<String, String> {
            &self.config
        }

        fn check(&self, _hashes: &HashCache) -> Result<UpdateStatus> {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(current, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            self.statuses
                .lock()
                .expect("fake update status lock poisoned")
                .pop_front()
                .unwrap_or(Ok(UpdateStatus::Unknown))
        }

        fn download(&self, _context: &mut DownloadContext<'_>) -> Result<PathBuf> {
            Err(Error::Http("download failed".to_owned()))
        }
    }

    fn app(name: &str, directory: &Path) -> InstalledApp {
        let image_path = directory.join(format!("{name}.AppImage"));

        InstalledApp {
            name: name.to_owned(),
            description: String::new(),
            version: None,
            image_path,
            desktop_path: directory.join(format!("{name}.desktop")),
            desktop_id: format!("{name}.desktop"),
            icon: None,
            icon_path: None,
            exec_arguments: String::new(),
            env_variables: Vec::new(),
            terminal: false,
            size: 1,
            external: false,
            self_entry: false,
        }
    }

    fn update_options(directory: &Path) -> UpdateOptions {
        UpdateOptions {
            appimages_dir: directory.join("AppImages"),
            applications_dir: directory.join("applications"),
            exec_name_for_terminal_apps: false,
            update_desktop_database: false,
            allow_unsafe_extractor: false,
        }
    }

    #[test]
    fn check_batch_runs_checks_concurrently_and_preserves_order() {
        let directory = tempfile::tempdir().unwrap();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let statuses = [
            UpdateStatus::Available,
            UpdateStatus::UpToDate,
            UpdateStatus::Unknown,
        ];
        let jobs: Vec<UpdateJob> = statuses
            .iter()
            .enumerate()
            .map(|(index, status)| {
                let manager = FakeManager::new([Ok(*status)]).with_delay(Duration::from_millis(40));
                let manager = FakeManager {
                    in_flight: Arc::clone(&in_flight),
                    max_in_flight: Arc::clone(&max_in_flight),
                    ..manager
                };
                UpdateJob::new(
                    app(&format!("app-{index}"), directory.path()),
                    Arc::new(manager),
                )
            })
            .collect();

        let report = check_batch(
            &jobs,
            &HashCache::new(directory.path().join("hashes")),
            3,
            Duration::ZERO,
            &mut |_| {},
        );

        assert_eq!(report.outcomes, statuses);
        assert!(
            max_in_flight.load(Ordering::SeqCst) >= 2,
            "checks should overlap"
        );
    }

    #[test]
    fn check_batch_retries_transient_failures() {
        let directory = tempfile::tempdir().unwrap();
        let manager = FakeManager::new([
            Err(Error::Http("transient".to_owned())),
            Ok(UpdateStatus::Available),
        ]);
        let jobs = vec![UpdateJob::new(
            app("retry", directory.path()),
            Arc::new(manager),
        )];
        let mut events = Vec::new();

        let report = check_batch(
            &jobs,
            &HashCache::new(directory.path().join("hashes")),
            1,
            Duration::from_secs(1),
            &mut |event| events.push(event),
        );

        assert_eq!(report.outcomes, vec![UpdateStatus::Available]);
        assert!(events.contains(&CheckEvent::Retrying));
        assert!(events.iter().any(|event| matches!(
            event,
            CheckEvent::Checked {
                status: UpdateStatus::Available,
                ..
            }
        )));
    }

    #[test]
    fn check_batch_reports_an_invalid_token_once() {
        let directory = tempfile::tempdir().unwrap();
        let jobs: Vec<UpdateJob> = (0..2)
            .map(|index| {
                UpdateJob::new(
                    app(&format!("token-{index}"), directory.path()),
                    Arc::new(FakeManager::new([Err(Error::GithubTokenInvalid)])),
                )
            })
            .collect();
        let mut invalid_events = 0;

        let report = check_batch(
            &jobs,
            &HashCache::new(directory.path().join("hashes")),
            2,
            Duration::ZERO,
            &mut |event| {
                if event == CheckEvent::TokenInvalid {
                    invalid_events += 1;
                }
            },
        );

        assert!(report.token_invalid);
        assert_eq!(report.outcomes, vec![UpdateStatus::Unknown; 2]);
        assert_eq!(invalid_events, 1);
    }

    #[test]
    fn check_batch_reports_rate_limit_on_every_run() {
        let directory = tempfile::tempdir().unwrap();
        let hashes = HashCache::new(directory.path().join("hashes"));
        let error = || {
            Error::HttpStatus(HttpStatus {
                url: "https://api.github.com/repos/o/r/releases/latest".to_owned(),
                status: 403,
                rate_limit_remaining: Some(0),
                rate_limit_reset: Some(1_700_000_000),
                retry_after: None,
            })
        };

        for run in 0..2 {
            let jobs = vec![UpdateJob::new(
                app(&format!("rate-{run}"), directory.path()),
                Arc::new(FakeManager::new([Err(error())])),
            )];
            let mut events = 0;

            let report = check_batch(&jobs, &hashes, 1, Duration::ZERO, &mut |event| {
                if matches!(event, CheckEvent::RateLimited(_)) {
                    events += 1;
                }
            });

            assert!(report.rate_limit.is_some(), "run {run}");
            assert_eq!(events, 1, "run {run} must show the notice again");
        }
    }

    #[test]
    fn apply_batch_reports_failures_without_stopping_other_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let jobs: Vec<UpdateJob> = (0..2)
            .map(|index| {
                UpdateJob::new(
                    app(&format!("batch-{index}"), directory.path()),
                    Arc::new(FakeManager::new([Ok(UpdateStatus::Available)])),
                )
            })
            .collect();
        let mut failed_events = 0;
        let mut config = Config::default();

        let report = apply_batch(
            &jobs,
            &update_options(directory.path()),
            &mut config,
            &directory.path().join("staging"),
            2,
            &Arc::new(AtomicBool::new(false)),
            &mut |event| {
                if matches!(event, BatchEvent::Failed { .. }) {
                    failed_events += 1;
                }
            },
        );

        assert!(report.installed.is_empty());
        assert_eq!(report.failed.len(), 2);
        assert!(!report.cancelled);
        assert_eq!(failed_events, 2);
    }
}
