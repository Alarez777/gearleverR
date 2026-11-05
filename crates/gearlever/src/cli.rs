//! Command line interface, behaviour-compatible with Gear Lever's CLI.

#![expect(
    unreachable_pub,
    reason = "the CLI is only used from main, but clap's derive generates public-looking items"
)]
#![expect(
    clippy::print_stdout,
    reason = "the CLI's standard output is its interface"
)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgGroup, Parser};
use serde::Serialize;

use gearlever_core::Error as CoreError;
use gearlever_core::appimage::{AppImage, MetadataOptions, is_running};
use gearlever_core::config::Config;
use gearlever_core::error::RateLimitInfo;
use gearlever_core::hash::HashCache;
use gearlever_core::install::{Conflict, IntegrateOptions, PreviousInstall, integrate};
use gearlever_core::paths::Paths;
use gearlever_core::scan::{InstalledApp, ScanOptions, scan};
use gearlever_core::uninstall::{Removal, uninstall};
use gearlever_core::update::{self, ManagerKind, UpdateOptions, UpdateStatus};

use crate::i18n::t;
use crate::rate_limit::{self, TokenState};
use crate::settings::Settings;
use crate::token_store::{TokenStatus, TokenStore};

/// Manage AppImages with ease.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the flags mirror the original CLI interface"
)]
#[derive(Debug, Parser)]
#[command(
    name = "gearlever-r",
    version,
    about = "Manage AppImages with ease",
    group = ArgGroup::new("command")
        .multiple(false)
        .args([
            "integrate",
            "update",
            "remove",
            "remove_all",
            "list_installed",
            "list_updates",
            "list_update_managers",
            "set_update_source",
            "set_update_url",
            "fetch_updates",
        ])
)]
pub struct Cli {
    /// Integrate an AppImage file
    #[arg(long, value_name = "FILE")]
    pub integrate: Option<PathBuf>,

    /// Update an AppImage file
    #[arg(long, value_name = "FILE")]
    pub update: Option<PathBuf>,

    /// Trashes an AppImage, its .desktop file and icons
    #[arg(long, value_name = "FILE")]
    pub remove: Option<PathBuf>,

    /// Removes all AppImages
    #[arg(long)]
    pub remove_all: bool,

    /// List integrated apps
    #[arg(long)]
    pub list_installed: bool,

    /// List available updates
    #[arg(long)]
    pub list_updates: bool,

    /// List available update managers
    #[arg(long)]
    pub list_update_managers: bool,

    /// Set a custom update source
    #[arg(long, value_name = "FILE")]
    pub set_update_source: Option<PathBuf>,

    /// Deprecated alias of --set-update-source for a static URL
    #[arg(long, value_name = "FILE")]
    pub set_update_url: Option<PathBuf>,

    /// Fetch updates in the background and send a desktop notification
    #[arg(long)]
    pub fetch_updates: bool,

    /// Prints a versioned, machine-readable JSON document
    #[arg(long)]
    pub json: bool,

    /// Skips any interactive question
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Forces updates for running applications
    #[arg(long)]
    pub force: bool,

    /// Updates all AppImages
    #[arg(long)]
    pub all: bool,

    /// Deletes the AppImage file from disk instead of trashing it
    #[arg(long)]
    pub delete: bool,

    /// If a name conflict occurs, keeps both files
    #[arg(long)]
    pub keep_both: bool,

    /// If a name conflict occurs, replaces the old file
    #[arg(long)]
    pub replace: bool,

    /// Set a custom URL for updates (deprecated)
    #[arg(long, value_name = "URL")]
    pub update_url: Option<String>,

    /// Update manager name, see --list-update-managers
    #[arg(long, value_name = "NAME")]
    pub manager: Option<String>,

    /// Unset a custom update source
    #[arg(long, requires = "set_update_source")]
    pub unset: bool,

    /// KEY=VALUE pairs for --set-update-source
    #[arg(value_name = "KEY=VALUE")]
    pub options: Vec<String>,
}

impl Cli {
    /// Returns whether a CLI command was requested.
    #[must_use]
    pub fn has_command(&self) -> bool {
        self.integrate.is_some()
            || self.update.is_some()
            || self.remove.is_some()
            || self.remove_all
            || self.list_installed
            || self.list_updates
            || self.list_update_managers
            || self.set_update_source.is_some()
            || self.set_update_url.is_some()
            || self.fetch_updates
    }

    /// Runs the requested command.
    ///
    /// # Errors
    ///
    /// Returns an error with context for any failed operation.
    pub fn run(mut self) -> Result<()> {
        let mut context = CommandContext::load()?;

        if let Some(file) = self.integrate.clone() {
            return self.integrate(&mut context, &file);
        }
        if self.all {
            return self.update_command(&mut context, None);
        }
        if let Some(file) = self.update.clone() {
            return self.update_command(&mut context, Some(&file));
        }
        if let Some(file) = self.remove.clone() {
            return self.remove_command(&mut context, &file);
        }
        if self.remove_all {
            return self.remove_all_command(&mut context);
        }
        if self.list_installed {
            return self.list_installed_command(&context);
        }
        if self.list_updates {
            return self.list_updates_command(&context);
        }
        if self.list_update_managers {
            Self::list_update_managers_command();
            return Ok(());
        }
        if let Some(file) = self.set_update_source.clone() {
            return self.set_update_source_command(&mut context, &file);
        }
        if let Some(file) = self.set_update_url.clone() {
            return self.set_update_url_command(&mut context, &file);
        }
        if self.fetch_updates {
            return Self::fetch_updates_command(&context);
        }

        Ok(())
    }

    fn integrate(&mut self, context: &mut CommandContext, file: &Path) -> Result<()> {
        let app_image = AppImage::open(file)
            .with_context(|| format!("{} is not a valid AppImage", file.display()))?;
        let metadata = app_image
            .metadata_with(&MetadataOptions {
                allow_unsafe_extractor: !context.settings.values().block_unsafe_extractor,
                work_dir: context.paths.temp_dir().join("metadata"),
            })
            .with_context(|| format!("cannot read the metadata of {}", file.display()))?;

        let apps = context.apps()?;
        if let Some(existing) = apps.iter().find(|app| same_file(&app.image_path, file)) {
            println!(
                "This AppImage is already integrated ({})",
                existing.image_path.display()
            );
            return Ok(());
        }

        let same_name = apps.iter().find(|app| app.name == metadata.name);
        let mut conflict = if self.replace {
            Conflict::Replace
        } else {
            Conflict::KeepBoth
        };
        let mut previous: Option<PreviousInstall> = None;

        if !self.yes {
            Self::print_table(&[
                vec!["Name".to_owned(), metadata.name.clone()],
                vec![
                    "Version".to_owned(),
                    metadata
                        .version
                        .clone()
                        .unwrap_or_else(|| "Not specified".to_owned()),
                ],
                vec![
                    "Description".to_owned(),
                    if metadata.comment.is_empty() {
                        "None".to_owned()
                    } else {
                        metadata.comment.clone()
                    },
                ],
                vec![
                    "Update Source".to_owned(),
                    update::resolve(app_image.path(), &BTreeMap::new(), None)
                        .ok()
                        .flatten()
                        .map_or_else(
                            || "None".to_owned(),
                            |manager| manager.kind().label().to_owned(),
                        ),
                ],
            ]);

            if !ask_yes_no("\nDo you really want to integrate this AppImage? (y/N)") {
                return Ok(());
            }

            if let Some(existing) = same_name {
                conflict = if ask_replace(&existing.name) {
                    Conflict::Replace
                } else {
                    Conflict::KeepBoth
                };
                if conflict == Conflict::Replace {
                    previous = Some(previous_from(existing));
                }
            }
        } else if self.replace {
            previous = same_name.map(previous_from);
        }

        let name_suffix = (conflict == Conflict::KeepBoth)
            .then(|| short_fingerprint(file))
            .transpose()?;

        let options = IntegrateOptions {
            appimages_dir: context.settings.values().appimages_dir.clone(),
            applications_dir: context.paths.applications_dir(),
            conflict,
            move_source: context.settings.values().move_on_integration,
            exec_name_for_terminal_apps: context.settings.values().exec_name_terminal,
            name_suffix,
            update_desktop_database: true,
        };

        let installed = integrate(
            file,
            &metadata,
            previous.as_ref(),
            &options,
            &mut context.config,
        )?;

        if let Some(url) = self.update_url.clone() {
            context.config.set_update_manager(
                &installed.image_path,
                ManagerKind::StaticFile.name(),
                BTreeMap::from([("url".to_owned(), url)]),
            );
        }

        context.save()?;

        println!(
            "{} was integrated successfully",
            installed.image_path.display()
        );

        Ok(())
    }

    fn remove_command(&mut self, context: &mut CommandContext, file: &Path) -> Result<()> {
        let apps = context.apps()?;
        let app = apps
            .iter()
            .find(|app| same_file(&app.image_path, file))
            .ok_or_else(|| anyhow!("Error: AppImage not integrated"))?;

        let mode = if self.delete {
            Removal::Delete
        } else {
            Removal::Trash
        };

        if !self.yes && !ask_yes_no("Do you really want to delete this AppImage? (y/N)") {
            return Ok(());
        }

        uninstall(app, mode, &mut context.config)?;
        context.save()?;

        println!("{} was removed successfully", app.image_path.display());

        Ok(())
    }

    fn remove_all_command(&mut self, context: &mut CommandContext) -> Result<()> {
        let apps = context.apps()?;
        if apps.is_empty() {
            println!("No integrated AppImages");
            return Ok(());
        }

        if !self.yes && !ask_yes_no("Do you really want to delete all AppImages? (y/N)") {
            return Ok(());
        }

        for app in &apps {
            println!("Removing {}", app.image_path.display());
            uninstall(app, Removal::Delete, &mut context.config)?;
        }
        context.save()?;

        Ok(())
    }

    fn list_installed_command(&self, context: &CommandContext) -> Result<()> {
        let apps = context.apps()?;

        if self.json {
            let installed: Vec<AppJson> = apps
                .iter()
                .map(|app| AppJson {
                    name: app.name.clone(),
                    path: app.image_path.to_string_lossy().into_owned(),
                    desktop_id: Some(app.desktop_id.clone()),
                    current_version: app.version.clone(),
                    available_version: None,
                    download_size: None,
                    manager: manager_name(&context.config, &app.image_path),
                    embedded_source: manager_embedded(&context.config, &app.image_path),
                    running: Some(is_running(&app.image_path)),
                })
                .collect();

            println!(
                "{}",
                serde_json::to_string(&ListJson {
                    schema_version: 1,
                    installed,
                })?
            );
            return Ok(());
        }

        let rows: Vec<Vec<String>> = apps
            .iter()
            .map(|app| {
                let version = app
                    .version
                    .clone()
                    .unwrap_or_else(|| "Not specified".to_owned());
                let manager = manager_name(&context.config, &app.image_path)
                    .unwrap_or_else(|| "UpdatesNotAvailable".to_owned());
                let embedded = manager_embedded(&context.config, &app.image_path);
                let manager = if embedded {
                    format!("[{manager}|embedded]")
                } else {
                    format!("[{manager}]")
                };

                vec![
                    app.name.clone(),
                    format!("[{version}]"),
                    manager,
                    app.image_path.to_string_lossy().into_owned(),
                ]
            })
            .collect();

        Self::print_table(&rows);
        Ok(())
    }

    fn list_update_managers_command() {
        let rows: Vec<Vec<String>> = ManagerKind::ALL
            .into_iter()
            .map(|kind| vec![kind.label().to_owned(), kind.name().to_owned()])
            .collect();

        Self::print_table(&rows);
    }

    fn list_updates_command(&self, context: &CommandContext) -> Result<()> {
        let updates = context.collect_updates()?.updates;

        if self.json {
            let entries: Vec<AppJson> = updates
                .iter()
                .map(|(app, manager)| AppJson {
                    name: app.name.clone(),
                    path: app.image_path.to_string_lossy().into_owned(),
                    desktop_id: Some(app.desktop_id.clone()),
                    current_version: app.version.clone(),
                    available_version: None,
                    download_size: None,
                    manager: Some(manager.kind().name().to_owned()),
                    embedded_source: manager.embedded().is_some(),
                    running: Some(is_running(&app.image_path)),
                })
                .collect();

            println!(
                "{}",
                serde_json::to_string(&UpdatesJson {
                    schema_version: 1,
                    updates: entries,
                })?
            );
            return Ok(());
        }

        if updates.is_empty() {
            println!("No updates available");
            return Ok(());
        }

        let rows: Vec<Vec<String>> = updates
            .iter()
            .map(|(app, manager)| {
                let embedded = if manager.embedded().is_some() {
                    "|embedded"
                } else {
                    ""
                };
                vec![
                    app.name.clone(),
                    format!("[Update available, {}{embedded}]", manager.kind().name()),
                    app.image_path.to_string_lossy().into_owned(),
                ]
            })
            .collect();

        Self::print_table(&rows);
        Ok(())
    }

    fn update_command(&mut self, context: &mut CommandContext, file: Option<&Path>) -> Result<()> {
        let updates = if let Some(file) = file {
            let apps = context.apps()?;
            let app = apps
                .iter()
                .find(|app| same_file(&app.image_path, file))
                .ok_or_else(|| anyhow!("Error: AppImage not integrated"))?;
            let manager = context.manager_for(&app.image_path)?;
            vec![(app.clone(), manager)]
        } else {
            let updates = context.collect_updates()?.updates;
            if updates.is_empty() {
                println!("No updates available");
                return Ok(());
            }
            if !self.yes && !ask_yes_no("\nDo you really want to update all AppImages? (y/N)") {
                return Ok(());
            }
            updates
        };

        for (app, manager) in updates {
            if is_running(&app.image_path) && !self.force {
                println!(
                    "{} was skipped because the application is running; use --force to skip this check.",
                    app.image_path.display()
                );
                continue;
            }

            if file.is_some()
                && !self.yes
                && !ask_yes_no("Do you really want to update this AppImage? (y/N)")
            {
                return Ok(());
            }

            println!("Downloading update from: {}", manager.kind().label());
            Self::install_update(context, &app, manager.as_ref())?;
        }

        Ok(())
    }

    fn install_update(
        context: &mut CommandContext,
        app: &InstalledApp,
        manager: &dyn update::UpdateManager,
    ) -> Result<()> {
        let options = context.update_options();
        let download_dir = context.paths.temp_dir().join("downloads");
        let mut progress = |fraction: f64| {
            print!("\rStatus: {}%", percent(fraction));
            let _ = io::stdout().flush();
        };

        let updated = update::apply(
            app,
            manager,
            &options,
            &mut context.config,
            &download_dir,
            &mut progress,
            Arc::new(AtomicBool::new(false)),
        )?;

        context.save()?;
        let _ = fs::remove_dir_all(context.paths.temp_dir());

        println!("\n{} updated successfully", updated.image_path.display());

        Ok(())
    }

    fn set_update_source_command(
        &mut self,
        context: &mut CommandContext,
        file: &Path,
    ) -> Result<()> {
        if self.unset {
            context.config.delete_update_manager(file);
            context.save()?;
            return Ok(());
        }

        let manager_name = self
            .manager
            .clone()
            .ok_or_else(|| anyhow!("Error: --manager is required"))?;
        let kind = ManagerKind::from_name(&manager_name)
            .ok_or_else(|| anyhow!("Error: \"{manager_name}\" is not a valid update manager"))?;

        let pairs = parse_pairs(&self.options);
        let mut values = BTreeMap::new();

        for field in kind.fields() {
            let raw = pairs
                .get(field.key)
                .ok_or_else(|| anyhow!("Error: missing --{}", field.key))?;

            let value = match field.kind {
                update::FieldKind::Text => raw.clone(),
                update::FieldKind::Boolean => match raw.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => "true".to_owned(),
                    "false" | "0" | "no" => "false".to_owned(),
                    _ => bail!("Error: {} must be true/false", field.key),
                },
                other => bail!(
                    "Error: {} has an unsupported field type ({other:?})",
                    field.key
                ),
            };

            values.insert(field.key.to_owned(), value);
        }

        validate_source(kind, &values)?;

        context.config.set_update_manager(file, kind.name(), values);
        context.save()?;

        println!("Update source set to {}", kind.label());

        Ok(())
    }

    fn set_update_url_command(&mut self, context: &mut CommandContext, file: &Path) -> Result<()> {
        let url = self
            .update_url
            .clone()
            .ok_or_else(|| anyhow!("Error: --set-update-url requires --update-url"))?;

        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("Error: --update-url must be an HTTP URL");
        }

        context.config.set_update_manager(
            file,
            ManagerKind::StaticFile.name(),
            BTreeMap::from([("url".to_owned(), url)]),
        );
        context.save()?;

        println!("Update source set to Static URL");

        Ok(())
    }

    fn fetch_updates_command(context: &CommandContext) -> Result<()> {
        let collected = context.collect_updates()?;

        if collected.token_invalid {
            Self::notify(&t(
                "GitHub authentication failed (401). Check your token in Preferences → GitHub.",
            ))?;
        }

        if let Some(info) = &collected.rate_limit {
            let message = rate_limit::message(info, collected.token_state);
            Self::notify(&message)?;
        }

        if collected.updates.is_empty() {
            println!("No updates available");
            return Ok(());
        }

        let message = if collected.updates.len() == 1 {
            "1 update available".to_owned()
        } else {
            format!("{} updates available", collected.updates.len())
        };

        Self::notify(&message)?;

        Ok(())
    }

    fn notify(message: &str) -> Result<()> {
        let result = Command::new("notify-send")
            .arg(message)
            .arg("--action=Open Gear LeverR")
            .arg("--expire-time=600000")
            .output();

        match result {
            Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "0" => {
                let executable = std::env::current_exe()?;
                let _ = Command::new(executable).spawn();
            }
            Ok(_) => println!("{message}"),
            Err(error) => {
                tracing::debug!(%error, "notify-send is not available");
                println!("{message}");
            }
        }

        Ok(())
    }

    fn print_table(rows: &[Vec<String>]) {
        if rows.is_empty() {
            return;
        }

        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0_usize; columns];

        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }

        for row in rows {
            let mut line = String::new();
            for (index, cell) in row.iter().enumerate() {
                if index > 0 {
                    line.push_str("  ");
                }
                line.push_str(cell);
                let padding = widths[index].saturating_sub(cell.chars().count());
                line.extend(std::iter::repeat_n(' ', padding));
            }
            println!("{}", line.trim_end());
        }
    }
}

/// Updates found by a CLI check and the account/token conditions encountered.
struct CollectedUpdates {
    updates: Vec<(InstalledApp, Box<dyn update::UpdateManager>)>,
    token_invalid: bool,
    rate_limit: Option<RateLimitInfo>,
    token_state: TokenState,
}

/// Shared state for a CLI invocation.
struct CommandContext {
    paths: Paths,
    settings: Settings,
    config: Config,
}

impl CommandContext {
    fn load() -> Result<Self> {
        let paths = Paths::from_env().context("cannot resolve the XDG directories")?;
        Config::import_legacy(&paths).context("cannot import the legacy configuration")?;
        let config = Config::load_or_default(&paths.config_file())
            .context("cannot read the configuration")?;
        let settings = Settings::load(&paths);

        Ok(Self {
            paths,
            settings,
            config,
        })
    }

    fn save(&self) -> Result<()> {
        self.config
            .save(&self.paths.config_file())
            .context("cannot write the configuration")
    }

    fn apps(&self) -> Result<Vec<InstalledApp>> {
        scan(
            &self.paths,
            &ScanOptions {
                appimages_dir: &self.settings.values().appimages_dir,
                include_external: self.settings.values().manage_outside,
            },
        )
        .context("cannot list integrated AppImages")
    }

    fn manager_for(&self, image: &Path) -> Result<Box<dyn update::UpdateManager>> {
        let config = self.config.update_manager(image);
        let token = TokenStore::new(self.settings.clone()).get();
        update::resolve(image, &config, token.as_deref())?
            .ok_or_else(|| anyhow!("No update method was found for this AppImage"))
    }

    fn collect_updates(&self) -> Result<CollectedUpdates> {
        let hashes = HashCache::new(self.paths.cache_root().join("hashes"));
        let token_store = TokenStore::new(self.settings.clone());
        let token = token_store.get();
        let token_state = if token.is_some() {
            TokenState::Used
        } else if token_store.status() != TokenStatus::None {
            TokenState::ConfiguredNotLoaded
        } else {
            TokenState::None
        };
        let mut notify_token_invalid = false;
        let mut rate_limit = None;
        let mut updates = Vec::new();

        for app in self.apps()? {
            let manager = match update::resolve(
                &app.image_path,
                &self.config.update_manager(&app.image_path),
                token.as_deref(),
            ) {
                Ok(Some(manager)) => manager,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        app = %app.name,
                        %error,
                        "cannot resolve the update manager"
                    );
                    continue;
                }
            };

            match manager.check(&hashes) {
                Ok(UpdateStatus::Available) => updates.push((app, manager)),
                Ok(_) => {}
                Err(error @ CoreError::GithubTokenInvalid) => {
                    tracing::warn!(
                        app = %app.name,
                        %error,
                        "update check failed"
                    );
                    if token_store.mark_invalid() {
                        notify_token_invalid = true;
                    }
                }
                Err(error) => {
                    let Some(delay) = error.retry_delay() else {
                        if let Some(info) = error.rate_limit_info() {
                            tracing::warn!(
                                app = %app.name,
                                %error,
                                "GitHub API rate limit exhausted"
                            );
                            rate_limit = Some(info);
                        } else {
                            tracing::warn!(
                                app = %app.name,
                                %error,
                                "update check failed"
                            );
                        }
                        continue;
                    };

                    tracing::debug!(
                        app = %app.name,
                        %error,
                        "update check failed, retrying"
                    );
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }

                    match manager.check(&hashes) {
                        Ok(UpdateStatus::Available) => updates.push((app, manager)),
                        Ok(_) => {}
                        Err(error) => {
                            if let Some(info) = error.rate_limit_info() {
                                tracing::warn!(
                                    app = %app.name,
                                    %error,
                                    "GitHub API rate limit exhausted after retry"
                                );
                                rate_limit = Some(info);
                            } else {
                                tracing::warn!(
                                    app = %app.name,
                                    %error,
                                    "update check failed after retry"
                                );
                            }
                        }
                    }
                }
            }
        }

        Ok(CollectedUpdates {
            updates,
            token_invalid: notify_token_invalid,
            rate_limit,
            token_state,
        })
    }

    fn update_options(&self) -> UpdateOptions {
        UpdateOptions {
            appimages_dir: self.settings.values().appimages_dir.clone(),
            applications_dir: self.paths.applications_dir(),
            exec_name_for_terminal_apps: self.settings.values().exec_name_terminal,
            update_desktop_database: true,
            allow_unsafe_extractor: !self.settings.values().block_unsafe_extractor,
        }
    }
}

#[derive(Debug, Serialize)]
struct AppJson {
    name: String,
    path: String,
    desktop_id: Option<String>,
    current_version: Option<String>,
    available_version: Option<String>,
    download_size: Option<u64>,
    manager: Option<String>,
    embedded_source: bool,
    running: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ListJson {
    schema_version: u32,
    installed: Vec<AppJson>,
}

#[derive(Debug, Serialize)]
struct UpdatesJson {
    schema_version: u32,
    updates: Vec<AppJson>,
}

fn manager_name(config: &Config, image: &Path) -> Option<String> {
    match update::resolve(image, &config.update_manager(image), None) {
        Ok(Some(manager)) => Some(manager.kind().name().to_owned()),
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "invalid update manager");
            None
        }
    }
}

fn manager_embedded(config: &Config, image: &Path) -> bool {
    update::resolve(image, &config.update_manager(image), None)
        .ok()
        .flatten()
        .is_some_and(|manager| manager.embedded().is_some())
}

fn previous_from(app: &InstalledApp) -> PreviousInstall {
    PreviousInstall {
        image_path: app.image_path.clone(),
        desktop_path: app.desktop_path.clone(),
        exec_arguments: app.exec_arguments.clone(),
        env_variables: app.env_variables.clone(),
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn short_fingerprint(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)?;
    let elapsed = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();

    let mut hasher = blake3::Hasher::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(&metadata.len().to_le_bytes());
    hasher.update(&elapsed.as_secs().to_le_bytes());
    hasher.update(&elapsed.subsec_nanos().to_le_bytes());

    let hex = hasher.finalize().to_hex();
    Ok(hex.as_str()[..6].to_owned())
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "progress is a fraction between 0 and 1"
)]
fn percent(fraction: f64) -> u32 {
    (fraction * 100.0).round() as u32
}

fn ask_yes_no(question: &str) -> bool {
    loop {
        print!("{question} ");
        let _ = io::stdout().flush();

        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            return false;
        }

        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return true,
            "n" | "no" | "" => return false,
            _ => {}
        }
    }
}

fn ask_replace(name: &str) -> bool {
    loop {
        print!(
            "\nAnother version of \"{name}\" is already integrated.\nHow do you want to proceed? ((K)eep both/(R)eplace): "
        );
        let _ = io::stdout().flush();

        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            return false;
        }

        match answer.trim().to_ascii_lowercase().as_str() {
            "k" | "keep" => return false,
            "r" | "replace" => return true,
            _ => {}
        }
    }
}

fn parse_pairs(options: &[String]) -> BTreeMap<String, String> {
    options
        .iter()
        .filter_map(|option| option.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn validate_source(kind: ManagerKind, values: &BTreeMap<String, String>) -> Result<()> {
    update::validate_config(kind, values).map_err(|error| anyhow!(error.to_string()))
}
