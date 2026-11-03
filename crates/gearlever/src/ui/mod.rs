//! GTK4 and libadwaita interface for Gear LeverR.
//!
//! The interface is built imperatively on top of a Relm4 [`Component`]: the
//! model holds plain data, the widgets struct holds the persistent shell, and
//! the list, details and multi-operation pages are rebuilt when they change.
//! Long operations run on threads and report back through messages, so the GTK
//! main loop is never blocked.

#![allow(
    deprecated,
    reason = "FileChooserDialog is deprecated only in GTK 4.10; staying on it keeps Debian 12 (GTK 4.8) supported"
)]

mod util;
mod welcome;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use relm4::adw::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::prelude::*;
use relm4::{adw, gtk};

use gearlever_core::Error as CoreError;
use gearlever_core::appimage::{AppImage, AppMetadata, MetadataOptions, detect, is_running};
use gearlever_core::cache::MetadataCache;
use gearlever_core::config::Config;
use gearlever_core::format::size;
use gearlever_core::hash::same_content;
use gearlever_core::install::{self, Conflict, IntegrateOptions, PreviousInstall};
use gearlever_core::paths::Paths;
use gearlever_core::scan::{InstalledApp, ScanOptions, scan};
use gearlever_core::self_install;
use gearlever_core::uninstall::{Removal, uninstall};
use gearlever_core::update::{self, ManagerKind, UpdateOptions, UpdateStatus};

use crate::i18n::{t, tf};
use crate::launch;
use crate::settings::{APP_ID, Settings, keys};
use util::{normalize, pill};

/// Starts the graphical application.
pub(crate) fn run(arguments: Vec<String>) {
    crate::i18n::init();
    tracing::info!("starting the Gear LeverR interface");

    let opened_from_file = !arguments.is_empty();
    let relm = RelmApp::new(APP_ID);

    let app = relm4::main_application();
    app.set_flags(app.flags() | gio::ApplicationFlags::HANDLES_OPEN);
    app.connect_open(|_, files, _| {
        let paths: Vec<PathBuf> = files
            .iter()
            .filter_map(gio::prelude::FileExt::path)
            .collect();
        if paths.is_empty() {
            return;
        }

        let sender = OPEN_SENDER.with(|slot| slot.borrow().clone());
        if let Some(sender) = sender {
            sender.input(Msg::OpenedFiles(paths));
        } else {
            PENDING_OPEN.with(|pending| pending.borrow_mut().extend(paths));
        }
    });

    let mut args = Vec::with_capacity(arguments.len() + 1);
    args.push(APP_ID.to_owned());
    args.extend(arguments);

    relm.with_args(args).run::<AppModel>(opened_from_file);
}

thread_local! {
    static OPEN_SENDER: RefCell<Option<ComponentSender<AppModel>>> = const { RefCell::new(None) };
    static PENDING_OPEN: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    List,
    Details,
    MultiInstall,
    MultiUpdate,
}

/// Which small window is used when an AppImage is opened from the file manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preview {
    Candidate,
    Installed,
}

/// Where the files came from: the external open signal or an in-app chooser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadSource {
    Chosen,
    External,
}

#[expect(
    clippy::large_enum_variant,
    reason = "the installed path is the common case and cloning is cheap enough"
)]
#[derive(Debug, Clone)]
enum DetailsItem {
    Installed(InstalledApp),
    Candidate {
        path: PathBuf,
        metadata: Box<AppMetadata>,
    },
}

/// One AppImage read from the command line, the chooser or a drop.
#[derive(Debug)]
struct LoadedFile {
    path: PathBuf,
    metadata: AppMetadata,
    installed: Option<PathBuf>,
}

#[derive(Debug)]
enum Msg {
    Refresh,
    ShowList,
    OpenPath(PathBuf),
    LeftButton,
    ToggleSearch,
    SearchChanged(String),
    OpenChooser,
    FilesChosen(Vec<PathBuf>),
    OpenedFiles(Vec<PathBuf>),
    FilesLoaded(Vec<LoadedFile>, Vec<String>),
    DragActive(bool),
    CheckUpdates,
    UpdateChecked(PathBuf, bool),
    CheckDone,
    InstallCandidate(PathBuf),
    InstallWithConflict {
        path: PathBuf,
        replace: bool,
    },
    InstallAll,
    Installed(Result<(InstalledApp, Config), String>),
    MultiInstalled(Result<Config, String>),
    Remove(PathBuf),
    RemoveConfirmed(PathBuf),
    Removed(Result<(PathBuf, Config), String>),
    RemoveAllConfirmed,
    AllRemoved {
        config: Config,
        errors: Vec<String>,
    },
    Launch(PathBuf),
    StartUpdate(PathBuf),
    StartAllUpdates,
    Progress(f64, String),
    CancelUpdate,
    UpdateCancelled,
    Updated(Result<(InstalledApp, Config), String>),
    MultiAppUpdated(PathBuf),
    MultiUpdated(Result<Config, String>, bool),
    SaveExec {
        path: PathBuf,
        env: Vec<String>,
        arguments: String,
    },
    SaveArguments {
        path: PathBuf,
        env: Vec<String>,
        arguments: String,
    },
    SaveWebsite {
        path: PathBuf,
        website: String,
    },
    ConfigSaved(Result<Config, String>),
    ReloadMetadata(PathBuf),
    MetadataReloaded(Result<(PathBuf, AppMetadata), String>),
    SelectSource(Option<ManagerKind>),
    ApplySource {
        path: PathBuf,
        kind: ManagerKind,
        values: BTreeMap<String, String>,
    },
    OpenFolder(PathBuf),
    OpenUrl(String),
    ShowPreferences,
    ShowAbout,
    ShowLog,
    Toast(String),
    Welcome,
    WelcomeFolder(PathBuf),
    WelcomeMove(bool),
    WelcomeFetchUpdates(bool),
    WelcomeInstall,
    WelcomeInstalled(Result<self_install::Installation, String>),
    WelcomeSkip,
    WelcomeClosed,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "the model tracks independent one-shot UI flags"
)]
struct AppModel {
    paths: Paths,
    settings: Settings,
    config: Config,
    apps: Vec<InstalledApp>,
    page: Page,
    details: Option<DetailsItem>,
    details_source: Option<ManagerKind>,
    search: String,
    updates: HashSet<PathBuf>,
    checking: bool,
    pending: Vec<(PathBuf, AppMetadata)>,
    multi_done: HashSet<PathBuf>,
    update_queue: Vec<InstalledApp>,
    opened_from_file: bool,
    external_open: bool,
    reloaded: Option<(PathBuf, AppMetadata)>,
    cancel_token: Option<Arc<AtomicBool>>,
    removing_self: bool,
}

struct Widgets {
    window: adw::ApplicationWindow,
    title: gtk::Label,
    left_button: gtk::Button,
    search_button: gtk::Button,
    menu_button: gtk::MenuButton,
    search_bar: gtk::SearchBar,
    stack: gtk::Stack,
    toast: adw::ToastOverlay,
    listbox: gtk::ListBox,
    list_stack: gtk::Stack,
    updates_button: gtk::Button,
    update_all_button: gtk::Button,
    details: gtk::Box,
    multi: gtk::Box,
    multi_progress: gtk::ProgressBar,
    progress: gtk::ProgressBar,
    cancel_button: gtk::Button,
    drop_revealer: gtk::Revealer,
    welcome: Option<welcome::Welcome>,
}

impl Component for AppModel {
    type CommandOutput = ();
    type Input = Msg;
    type Output = ();
    type Init = bool;
    type Root = adw::ApplicationWindow;
    type Widgets = Widgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title("Gear LeverR")
            .default_width(720)
            .default_height(720)
            .build()
    }

    fn init(
        opened_from_file: bool,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        util::install_css();

        let mut model = Self::new();
        model.opened_from_file = opened_from_file;

        OPEN_SENDER.with(|slot| *slot.borrow_mut() = Some(sender.clone()));
        let pending =
            PENDING_OPEN.with(|pending| pending.borrow_mut().drain(..).collect::<Vec<_>>());

        let widgets = Self::build_shell(&root, &sender);
        Self::rebuild_list(&model, &widgets);
        Self::show_page(&model, &widgets);

        if model.settings.values().is_maximized {
            root.maximize();
        }

        if !pending.is_empty() {
            sender.input(Msg::OpenedFiles(pending));
        } else if !opened_from_file && model.should_show_welcome() {
            sender.input(Msg::Welcome);
        }

        ComponentParts { model, widgets }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one dispatch point keeps the message flow easy to follow"
    )]
    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        let _ = root;

        match message {
            Msg::Refresh => {
                self.settings = Settings::load(&self.paths);
                self.refresh_apps();
                Self::rebuild_list(self, widgets);
                Self::rebuild_details(self, widgets, &sender);
            }
            Msg::ShowList => {
                self.external_open = false;
                self.page = Page::List;
                self.details = None;
                self.reloaded = None;
                Self::show_page(self, widgets);
                Self::rebuild_list(self, widgets);
            }
            Msg::OpenPath(path) => {
                self.external_open = false;
                if let Some(app) = self.apps.iter().find(|app| app.image_path == path) {
                    self.details = Some(DetailsItem::Installed(app.clone()));
                    self.details_source = None;
                    self.reloaded = None;
                    self.page = Page::Details;
                    Self::show_page(self, widgets);
                    Self::rebuild_details(self, widgets, &sender);
                }
            }
            Msg::LeftButton => {
                if self.page == Page::List {
                    sender.input(Msg::OpenChooser);
                } else {
                    sender.input(Msg::ShowList);
                }
            }
            Msg::ToggleSearch => {
                let open = !widgets.search_bar.is_search_mode();
                widgets.search_bar.set_search_mode(open);
                if open {
                    widgets.search_bar.grab_focus();
                }
            }
            Msg::SearchChanged(query) => {
                self.search = query;
                Self::rebuild_list(self, widgets);
            }
            Msg::OpenChooser => Self::open_chooser(root, &sender),
            Msg::OpenedFiles(paths) => {
                widgets.window.present();
                self.files_chosen(widgets, &sender, paths, LoadSource::External);
            }
            Msg::FilesChosen(paths) => {
                self.files_chosen(widgets, &sender, paths, LoadSource::Chosen);
            }
            Msg::FilesLoaded(loaded, errors) => {
                for error in errors {
                    Self::toast(widgets, &error);
                }

                let mut fresh = Vec::new();
                let mut already_installed = 0;

                for file in loaded {
                    let installed = file.installed.or_else(|| {
                        self.apps
                            .iter()
                            .find(|app| same_file(&app.image_path, &file.path))
                            .map(|app| app.image_path.clone())
                    });

                    if let Some(installed) = installed {
                        if let Some(app) = self.apps.iter().find(|app| app.image_path == installed)
                        {
                            already_installed += 1;
                            if self.apps.len() == 1 || already_installed == 1 {
                                self.details = Some(DetailsItem::Installed(app.clone()));
                                self.details_source = None;
                                self.reloaded = None;
                            }
                        }
                    } else {
                        fresh.push((file.path, file.metadata));
                    }
                }

                if already_installed > 0 && fresh.is_empty() {
                    self.page = Page::Details;
                    Self::show_page(self, widgets);
                    if self.minimal_preview() {
                        Self::minimal_ui(widgets, Preview::Installed);
                    }
                    Self::rebuild_details(self, widgets, &sender);
                    return;
                }

                match fresh.len() {
                    0 => {}
                    1 => {
                        if let Some((path, metadata)) = fresh.into_iter().next() {
                            self.details = Some(DetailsItem::Candidate {
                                path,
                                metadata: Box::new(metadata),
                            });
                            self.details_source = None;
                            self.reloaded = None;
                            self.page = Page::Details;
                            Self::show_page(self, widgets);
                            Self::rebuild_details(self, widgets, &sender);
                            if self.minimal_preview() {
                                Self::minimal_ui(widgets, Preview::Candidate);
                            }
                        }
                    }
                    _ => {
                        self.pending = fresh;
                        self.page = Page::MultiInstall;
                        Self::show_page(self, widgets);
                        Self::rebuild_multi(self, widgets, &sender);
                    }
                }
            }
            Msg::DragActive(active) => {
                if active {
                    widgets.drop_revealer.set_visible(true);
                    widgets.drop_revealer.set_reveal_child(true);
                } else {
                    widgets.drop_revealer.set_reveal_child(false);
                    widgets.drop_revealer.set_visible(false);
                }
            }
            Msg::CheckUpdates => {
                if self.checking {
                    return;
                }
                self.checking = true;
                widgets.updates_button.set_sensitive(false);
                widgets.updates_button.set_label(&t("Checking updates..."));

                let apps = self.apps.clone();
                let config = self.config.clone();
                let hash_dir = self.paths.cache_root().join("hashes");

                std::thread::spawn(move || {
                    for chunk in apps.chunks(8) {
                        std::thread::scope(|scope| {
                            for app in chunk {
                                let sender = sender.clone();
                                let config = config.clone();
                                let hash_dir = hash_dir.clone();
                                scope.spawn(move || {
                                    let available = update::resolve(
                                        &app.image_path,
                                        &config.update_manager(&app.image_path),
                                    )
                                    .ok()
                                    .flatten()
                                    .and_then(|manager| {
                                        manager
                                            .check(&gearlever_core::hash::HashCache::new(hash_dir))
                                            .ok()
                                    })
                                    .is_some_and(|status| status == UpdateStatus::Available);

                                    sender.input(Msg::UpdateChecked(
                                        app.image_path.clone(),
                                        available,
                                    ));
                                });
                            }
                        });
                    }

                    sender.input(Msg::CheckDone);
                });
            }
            Msg::UpdateChecked(path, available) => {
                if available {
                    self.updates.insert(path.clone());
                } else {
                    self.updates.remove(&path);
                }

                if let Some(DetailsItem::Installed(app)) = &self.details
                    && app.image_path == path
                {
                    Self::rebuild_details(self, widgets, &sender);
                }
            }
            Msg::CheckDone => {
                self.checking = false;
                widgets.updates_button.set_sensitive(true);
                widgets
                    .updates_button
                    .set_label(if self.updates.is_empty() {
                        "No updates found"
                    } else {
                        "Check for updates"
                    });
                Self::rebuild_list(self, widgets);
            }
            Msg::InstallCandidate(path) => {
                let Some(metadata) = self.pending_metadata(&path).cloned() else {
                    Self::toast(widgets, &t("Cannot read the AppImage metadata"));
                    return;
                };

                if self.apps.iter().any(|app| {
                    app.name == metadata.name
                        || (self_install::is_self_metadata(&metadata) && app.self_entry)
                }) {
                    Self::confirm_conflict(widgets, path, &metadata.name, &sender);
                    return;
                }

                Self::start_install(self, widgets, path, Conflict::KeepBoth, &sender);
            }
            Msg::InstallWithConflict { path, replace } => {
                let conflict = if replace {
                    Conflict::Replace
                } else {
                    Conflict::KeepBoth
                };
                Self::start_install(self, widgets, path, conflict, &sender);
            }
            Msg::InstallAll => {
                let pending = self.pending.clone();
                let options = self.integrate_options(Conflict::KeepBoth, None);
                let config = self.config.clone();
                let total = pending.len();
                widgets.progress.set_visible(true);

                std::thread::spawn(move || {
                    let mut config = config;
                    for (index, (path, metadata)) in pending.iter().enumerate() {
                        if let Err(error) =
                            install::integrate(path, metadata, None, &options, &mut config)
                        {
                            sender.input(Msg::Toast(format!("{}: {error}", path.display())));
                            continue;
                        }
                        sender.input(Msg::Progress(
                            fraction(index + 1, total),
                            tf(
                                "Integrated {done} of {total}",
                                &[
                                    ("done", &(index + 1).to_string()),
                                    ("total", &total.to_string()),
                                ],
                            ),
                        ));
                    }
                    sender.input(Msg::MultiInstalled(Ok(config)));
                });
            }
            Msg::Installed(result) => {
                widgets.progress.set_visible(false);
                match result {
                    Ok((app, config)) => {
                        self.config = config;
                        let _ = self.config.save(&self.paths.config_file());
                        self.pending.clear();
                        self.reloaded = None;
                        self.refresh_apps();
                        let installed = self
                            .apps
                            .iter()
                            .find(|candidate| candidate.image_path == app.image_path)
                            .cloned();

                        // Replacing the self entry means this process is still
                        // running the old version, so hand over to the image
                        // that was just installed.
                        if let Some(installed) = &installed
                            && installed.self_entry
                            && Self::restart_into(widgets, &installed.image_path, None)
                        {
                            return;
                        }

                        self.details = installed.map(DetailsItem::Installed);
                        self.page = Page::Details;
                        Self::show_page(self, widgets);
                        Self::rebuild_details(self, widgets, &sender);
                        Self::toast(widgets, &t("AppImage integrated"));
                    }
                    Err(error) => Self::toast(widgets, &error),
                }
            }
            Msg::MultiInstalled(result) => {
                widgets.progress.set_visible(false);
                match result {
                    Ok(config) => {
                        self.config = config;
                        let _ = self.config.save(&self.paths.config_file());
                        self.pending.clear();
                        self.page = Page::List;
                        self.refresh_apps();
                        Self::show_page(self, widgets);
                        Self::rebuild_list(self, widgets);
                        Self::toast(widgets, &t("AppImages integrated"));
                    }
                    Err(error) => Self::toast(widgets, &error),
                }
            }
            Msg::Remove(path) => {
                let Some(app) = self.apps.iter().find(|app| app.image_path == path) else {
                    return;
                };
                if app.self_entry {
                    Self::confirm_self_remove(widgets, path, self.apps.len(), &sender);
                } else {
                    Self::confirm_remove(widgets, path, &sender);
                }
            }
            Msg::RemoveConfirmed(path) => {
                let Some(app) = self.apps.iter().find(|app| app.image_path == path).cloned() else {
                    return;
                };

                self.removing_self = app.self_entry;
                let config = self.config.clone();

                std::thread::spawn(move || {
                    let mut config = config;
                    let result = uninstall(&app, Removal::Trash, &mut config)
                        .map(|_| (app.image_path.clone(), config))
                        .map_err(|error| error.to_string());
                    sender.input(Msg::Removed(result));
                });
            }
            Msg::Removed(result) => match result {
                Ok((path, config)) => {
                    self.config = config;
                    let _ = self.config.save(&self.paths.config_file());

                    if self.removing_self {
                        self.removing_self = false;
                        widgets.window.close();
                        return;
                    }

                    if self.opened_from_file {
                        widgets.window.close();
                        return;
                    }

                    self.page = Page::List;
                    self.details = None;
                    self.refresh_apps();
                    Self::show_page(self, widgets);
                    Self::rebuild_list(self, widgets);
                    Self::toast(
                        widgets,
                        &tf("Removed {path}", &[("path", &path.display().to_string())]),
                    );
                }
                Err(error) => {
                    self.removing_self = false;
                    Self::toast(widgets, &error);
                }
            },
            Msg::RemoveAllConfirmed => {
                let apps = self.apps.clone();
                if apps.is_empty() {
                    return;
                }

                let config = self.config.clone();
                std::thread::spawn(move || {
                    let mut config = config;
                    let mut errors = Vec::new();

                    for app in &apps {
                        if let Err(error) = uninstall(app, Removal::Trash, &mut config) {
                            errors.push(format!("{}: {error}", app.name));
                        }
                    }

                    sender.input(Msg::AllRemoved { config, errors });
                });
            }
            Msg::AllRemoved { config, errors } => {
                self.config = config;
                let _ = self.config.save(&self.paths.config_file());

                if !self_install::is_installed(&self.paths) {
                    widgets.window.close();
                    return;
                }

                self.refresh_apps();
                Self::rebuild_list(self, widgets);
                if errors.is_empty() {
                    Self::toast(widgets, &t("AppImages removed"));
                } else {
                    Self::toast(widgets, &errors.join("\n"));
                }
            }
            Msg::Launch(path) => {
                if let Some(app) = self.apps.iter().find(|app| app.image_path == path) {
                    if is_self_launch(app) {
                        Self::restart(widgets, app);
                    } else {
                        Self::launch(app);
                    }
                } else if self
                    .pending_metadata(&path)
                    .is_some_and(self_install::is_self_metadata)
                {
                    Self::restart_candidate(widgets, &path);
                } else {
                    Self::launch_file(&path);
                }
            }
            Msg::StartUpdate(path) => {
                if self.cancel_token.is_some() {
                    Self::toast(widgets, &t("An update is already running"));
                    return;
                }

                let Some(app) = self.apps.iter().find(|app| app.image_path == path).cloned() else {
                    return;
                };

                if is_running(&app.image_path) {
                    Self::toast(widgets, &t("The application is running, close it first"));
                    return;
                }

                let Ok(manager) = self.manager_for(&app.image_path) else {
                    Self::toast(widgets, &t("No update method was found for this AppImage"));
                    return;
                };

                let options = self.update_options();
                let config = self.config.clone();
                let download_dir = self.paths.temp_dir().join("downloads");
                let cancel = Arc::new(AtomicBool::new(false));
                self.cancel_token = Some(Arc::clone(&cancel));
                Self::show_progress(widgets);

                std::thread::spawn(move || {
                    let mut config = config;
                    let mut progress = |fraction: f64| {
                        sender.input(Msg::Progress(
                            fraction,
                            tf("Updating {name}...", &[("name", &app.name)]),
                        ));
                    };

                    let result = update::apply(
                        &app,
                        manager.as_ref(),
                        &options,
                        &mut config,
                        &download_dir,
                        &mut progress,
                        cancel,
                    );

                    match result {
                        Ok(app) => sender.input(Msg::Updated(Ok((app, config)))),
                        Err(CoreError::Cancelled) => sender.input(Msg::UpdateCancelled),
                        Err(error) => sender.input(Msg::Updated(Err(error.to_string()))),
                    }
                });
            }
            Msg::StartAllUpdates => {
                if self.cancel_token.is_some() {
                    Self::toast(widgets, &t("An update is already running"));
                    return;
                }

                let queue: Vec<InstalledApp> = self
                    .apps
                    .iter()
                    .filter(|app| self.updates.contains(&app.image_path))
                    .cloned()
                    .collect();

                if queue.is_empty() {
                    Self::toast(widgets, &t("No updates available"));
                    return;
                }

                self.multi_done.clear();
                self.update_queue.clone_from(&queue);
                self.page = Page::MultiUpdate;
                Self::show_page(self, widgets);
                Self::rebuild_multi(self, widgets, &sender);

                let config = self.config.clone();
                let options = self.update_options();
                let download_dir = self.paths.temp_dir().join("downloads");
                let cancel = Arc::new(AtomicBool::new(false));
                self.cancel_token = Some(Arc::clone(&cancel));
                widgets.multi_progress.set_visible(true);
                widgets.multi_progress.set_fraction(0.0);

                std::thread::spawn(move || {
                    let mut config = config;
                    let total = queue.len();
                    let mut cancelled = false;

                    for (index, app) in queue.iter().enumerate() {
                        if cancel.load(Ordering::Relaxed) {
                            cancelled = true;
                            break;
                        }

                        let Some(manager) = update::resolve(
                            &app.image_path,
                            &config.update_manager(&app.image_path),
                        )
                        .ok()
                        .flatten() else {
                            continue;
                        };

                        let base = fraction(index, total);
                        let total_pages = fraction(total, 1);
                        let sender_progress = sender.clone();
                        let name = app.name.clone();
                        let mut progress = |progress: f64| {
                            sender_progress.input(Msg::Progress(
                                (base + progress / total_pages).min(1.0),
                                tf("Updating {name}...", &[("name", &name)]),
                            ));
                        };

                        match update::apply(
                            app,
                            manager.as_ref(),
                            &options,
                            &mut config,
                            &download_dir,
                            &mut progress,
                            Arc::clone(&cancel),
                        ) {
                            Ok(_) => sender.input(Msg::MultiAppUpdated(app.image_path.clone())),
                            Err(CoreError::Cancelled) => {
                                cancelled = true;
                                break;
                            }
                            Err(error) => {
                                sender.input(Msg::Toast(format!("{}: {error}", app.name)));
                            }
                        }
                    }

                    sender.input(Msg::MultiUpdated(Ok(config), cancelled));
                });
            }
            Msg::Progress(fraction, text) => {
                if self.page == Page::MultiUpdate {
                    widgets.multi_progress.set_visible(true);
                    widgets.multi_progress.set_fraction(fraction);
                } else {
                    Self::show_progress(widgets);
                    widgets.progress.set_fraction(fraction);
                    widgets.progress.set_text(Some(&text));
                }
            }
            Msg::CancelUpdate => {
                if let Some(cancel) = &self.cancel_token {
                    cancel.store(true, Ordering::Relaxed);
                    if self.page != Page::MultiUpdate {
                        widgets.cancel_button.set_sensitive(false);
                        widgets.cancel_button.set_label(&t("Cancelling..."));
                    }
                }
            }
            Msg::UpdateCancelled => {
                self.cancel_token = None;
                Self::hide_progress(widgets);
                Self::toast(widgets, &t("Update cancelled"));
            }
            Msg::Updated(result) => {
                self.cancel_token = None;
                Self::hide_progress(widgets);
                match result {
                    Ok((app, config)) => {
                        self.config = config;
                        let _ = self.config.save(&self.paths.config_file());
                        self.updates.remove(&app.image_path);
                        self.reloaded = None;
                        self.refresh_apps();
                        self.details = self
                            .apps
                            .iter()
                            .find(|candidate| candidate.image_path == app.image_path)
                            .cloned()
                            .map(DetailsItem::Installed);
                        Self::rebuild_details(self, widgets, &sender);
                        Self::rebuild_list(self, widgets);
                        Self::toast(widgets, &tf("{name} updated", &[("name", &app.name)]));
                    }
                    Err(error) => Self::toast(widgets, &error),
                }
            }
            Msg::MultiAppUpdated(path) => {
                self.multi_done.insert(path.clone());
                self.updates.remove(&path);
                if self.page == Page::MultiUpdate {
                    Self::rebuild_multi(self, widgets, &sender);
                }
            }
            Msg::MultiUpdated(result, cancelled) => {
                self.cancel_token = None;
                widgets.multi_progress.set_visible(false);
                Self::hide_progress(widgets);
                match result {
                    Ok(config) => {
                        self.config = config;
                        let _ = self.config.save(&self.paths.config_file());
                        // Clear the badges before rebuilding the list; otherwise
                        // the rows keep the update icon that was already applied.
                        if !cancelled {
                            self.updates.clear();
                        }
                        self.multi_done.clear();
                        self.update_queue.clear();
                        self.page = Page::List;
                        self.refresh_apps();
                        Self::show_page(self, widgets);
                        Self::rebuild_list(self, widgets);
                        if cancelled {
                            Self::toast(widgets, &t("Updates cancelled"));
                        } else {
                            Self::toast(widgets, &t("Updates installed"));
                        }
                    }
                    Err(error) => Self::toast(widgets, &error),
                }
            }
            Msg::SaveExec {
                path,
                env,
                arguments,
            } => {
                if let Some(app) = self.apps.iter().find(|app| app.image_path == path).cloned() {
                    match install::set_exec(&app, &env, &arguments) {
                        Ok(()) => {
                            self.refresh_apps();
                            self.details = self
                                .apps
                                .iter()
                                .find(|candidate| candidate.image_path == path)
                                .cloned()
                                .map(DetailsItem::Installed);
                            Self::rebuild_details(self, widgets, &sender);
                        }
                        Err(error) => Self::toast(widgets, &error.to_string()),
                    }
                }
            }
            Msg::SaveArguments {
                path,
                env,
                arguments,
            } => {
                if let Some(app) = self.apps.iter().find(|app| app.image_path == path).cloned() {
                    match install::set_exec(&app, &env, &arguments) {
                        Ok(()) => {
                            if let Some(app) = self
                                .apps
                                .iter_mut()
                                .find(|candidate| candidate.image_path == path)
                            {
                                app.exec_arguments.clone_from(&arguments);
                            }
                            if let Some(DetailsItem::Installed(app)) = &mut self.details
                                && app.image_path == path
                            {
                                app.exec_arguments.clone_from(&arguments);
                            }
                        }
                        Err(error) => Self::toast(widgets, &error.to_string()),
                    }
                }
            }
            Msg::SaveWebsite { path, website } => {
                if !website.is_empty()
                    && !website.starts_with("http://")
                    && !website.starts_with("https://")
                {
                    return;
                }

                let mut values = self.config.app(&path);
                if website.is_empty() {
                    values.remove("website");
                } else {
                    values.insert("website".to_owned(), website);
                }
                self.config.set_app(&path, values);
                if let Err(error) = self.config.save(&self.paths.config_file()) {
                    Self::toast(widgets, &error.to_string());
                }
            }
            Msg::ConfigSaved(result) => match result {
                Ok(config) => {
                    self.config = config;
                    Self::rebuild_details(self, widgets, &sender);
                    Self::toast(widgets, &t("Update source saved"));
                    sender.input(Msg::CheckUpdates);
                }
                Err(error) => Self::toast(widgets, &error),
            },
            Msg::ReloadMetadata(path) => {
                let cache_dir = self.paths.metadata_cache_dir();
                let allow_unsafe_extractor = !self.settings.values().block_unsafe_extractor;
                let work_dir = self.paths.temp_dir().join("metadata");
                std::thread::spawn(move || {
                    let options = MetadataOptions {
                        allow_unsafe_extractor,
                        work_dir,
                    };

                    let result = AppImage::open(&path)
                        .and_then(|appimage| appimage.metadata_with(&options))
                        .and_then(|metadata| {
                            MetadataCache::new(cache_dir).store(&path, &metadata)?;
                            Ok((path.clone(), metadata))
                        })
                        .map_err(|error| error.to_string());
                    sender.input(Msg::MetadataReloaded(result));
                });
            }
            Msg::MetadataReloaded(result) => match result {
                Ok((path, metadata)) => {
                    self.reloaded = Some((path, metadata));
                    self.refresh_apps();
                    Self::rebuild_details(self, widgets, &sender);
                    Self::rebuild_list(self, widgets);
                    Self::toast(widgets, &t("Metadata reloaded"));
                }
                Err(error) => Self::toast(widgets, &error),
            },
            Msg::SelectSource(kind) => {
                if kind.is_none()
                    && let Some(DetailsItem::Installed(app)) = &self.details
                {
                    let path = app.image_path.clone();
                    self.config.delete_update_manager(&path);
                    let _ = self.config.save(&self.paths.config_file());
                    self.details_source = None;
                    Self::rebuild_details(self, widgets, &sender);
                    sender.input(Msg::CheckUpdates);
                    return;
                }

                self.details_source = kind;
                Self::rebuild_details(self, widgets, &sender);
            }
            Msg::ApplySource { path, kind, values } => {
                let mut config = self.config.clone();
                config.set_update_manager(&path, kind.name(), values);
                match config.save(&self.paths.config_file()) {
                    Ok(()) => {
                        sender.input(Msg::ConfigSaved(Ok(config)));
                    }
                    Err(error) => Self::toast(widgets, &error.to_string()),
                }
            }
            Msg::OpenFolder(path) => {
                if let Some(parent) = path.parent() {
                    let uri = gio::File::for_path(parent).uri();
                    Self::open_uri(uri.as_str());
                }
            }
            Msg::OpenUrl(url) => Self::open_uri(&url),
            Msg::ShowPreferences => Self::show_preferences(self, widgets, &sender),
            Msg::ShowAbout => Self::show_about(&widgets.window),
            Msg::ShowLog => {
                let uri = gio::File::for_path(self.paths.log_file()).uri();
                Self::open_uri(uri.as_str());
            }
            Msg::Toast(message) => Self::toast(widgets, &message),
            Msg::Welcome => {
                widgets.window.present();
                widgets.welcome = Some(welcome::present(&widgets.window, &self.settings, &sender));
            }
            Msg::WelcomeFolder(path) => {
                self.settings.set_appimages_dir(&path);
                if let Some(welcome) = &widgets.welcome {
                    welcome.folder_row.set_subtitle(&path.to_string_lossy());
                }
                self.refresh_apps();
                Self::rebuild_list(self, widgets);
            }
            Msg::WelcomeMove(move_source) => {
                self.settings
                    .set_boolean(keys::MOVE_ON_INTEGRATION, move_source);
            }
            Msg::WelcomeFetchUpdates(enabled) => {
                match crate::autostart::apply(&self.paths, enabled) {
                    Ok(()) => self
                        .settings
                        .set_boolean(keys::FETCH_IN_BACKGROUND, enabled),
                    Err(error) => {
                        if let Some(welcome) = &widgets.welcome {
                            welcome.revert_fetch_updates();
                            welcome.toast(&error);
                        }
                    }
                }
            }
            Msg::WelcomeInstall => {
                let Some(image) = running_appimage() else {
                    if let Some(welcome) = &widgets.welcome {
                        welcome.toast(&t("Cannot determine the running AppImage"));
                    }
                    return;
                };

                let options = self_install::InstallOptions {
                    appimages_dir: self.settings.values().appimages_dir.clone(),
                    applications_dir: self.paths.applications_dir(),
                    icons_dir: self.paths.hicolor_icons_dir(),
                    move_source: self.settings.values().move_on_integration,
                    update_desktop_database: true,
                };

                if let Some(welcome) = &widgets.welcome {
                    welcome.begin_install();
                }

                std::thread::spawn(move || {
                    let result =
                        self_install::install(&image, &options).map_err(|error| error.to_string());
                    sender.input(Msg::WelcomeInstalled(result));
                });
            }
            Msg::WelcomeInstalled(result) => match result {
                Ok(installation) => {
                    if let Some(welcome) = widgets.welcome.take() {
                        welcome.window.close();
                    }
                    tracing::info!(
                        image = %installation.image_path.display(),
                        desktop = %installation.desktop_path.display(),
                        "Gear LeverR installed"
                    );
                    self.refresh_apps();
                    Self::rebuild_list(self, widgets);
                    Self::toast(widgets, &t("Gear LeverR installed"));
                }
                Err(error) => {
                    if let Some(welcome) = &widgets.welcome {
                        welcome.end_install();
                        welcome.toast(&error);
                    }
                }
            },
            Msg::WelcomeSkip => {
                if let Some(welcome) = widgets.welcome.take() {
                    welcome.window.close();
                }
            }
            Msg::WelcomeClosed => {
                widgets.welcome = None;
            }
        }
    }
}

impl AppModel {
    fn new() -> Self {
        let paths = Paths::from_env().unwrap_or_else(|_| Paths::with_home(glib::home_dir()));
        let settings = Settings::load(&paths);
        let _ = Config::import_legacy(&paths);
        if self_install::is_installed(&paths)
            && let Err(error) = self_install::refresh_entry(&paths)
        {
            tracing::warn!(%error, "cannot refresh the Gear LeverR desktop entry");
        }
        let config = Config::load_or_default(&paths.config_file()).unwrap_or_default();

        let mut model = Self {
            paths,
            settings,
            config,
            apps: Vec::new(),
            page: Page::List,
            details: None,
            details_source: None,
            search: String::new(),
            updates: HashSet::new(),
            checking: false,
            pending: Vec::new(),
            multi_done: HashSet::new(),
            update_queue: Vec::new(),
            opened_from_file: false,
            external_open: false,
            reloaded: None,
            cancel_token: None,
            removing_self: false,
        };

        model.refresh_apps();
        model
    }

    /// Returns whether the first-run window should be shown.
    fn should_show_welcome(&self) -> bool {
        !self.opened_from_file
            && running_appimage().is_some()
            && !self_install::is_installed(&self.paths)
    }

    /// Returns whether a file opened from the file manager uses the small preview.
    fn minimal_preview(&self) -> bool {
        self.opened_from_file && self.settings.values().preview_minimal_ui
    }

    fn refresh_apps(&mut self) {
        let directory = self.settings.values().appimages_dir.clone();
        let include_external = self.settings.values().manage_outside;

        match scan(
            &self.paths,
            &ScanOptions {
                appimages_dir: &directory,
                include_external,
            },
        ) {
            Ok(apps) => self.apps = apps,
            Err(error) => tracing::error!(%error, "cannot list integrated AppImages"),
        }
    }

    fn pending_metadata(&self, path: &Path) -> Option<&AppMetadata> {
        if let Some(DetailsItem::Candidate {
            path: candidate,
            metadata,
        }) = &self.details
            && candidate == path
        {
            return Some(metadata);
        }

        self.pending
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, metadata)| metadata)
    }

    fn integrate_options(
        &self,
        conflict: Conflict,
        name_suffix: Option<String>,
    ) -> IntegrateOptions {
        IntegrateOptions {
            appimages_dir: self.settings.values().appimages_dir.clone(),
            applications_dir: self.paths.applications_dir(),
            conflict,
            move_source: self.settings.values().move_on_integration,
            exec_name_for_terminal_apps: self.settings.values().exec_name_terminal,
            name_suffix,
            update_desktop_database: true,
        }
    }

    fn start_install(
        &self,
        widgets: &Widgets,
        path: PathBuf,
        conflict: Conflict,
        sender: &ComponentSender<Self>,
    ) {
        let Some(metadata) = self.pending_metadata(&path).cloned() else {
            Self::toast(widgets, &t("Cannot read the AppImage metadata"));
            return;
        };

        let previous = if conflict == Conflict::Replace {
            let self_candidate = self_install::is_self_metadata(&metadata);
            let preferred = if self_candidate {
                self.apps.iter().find(|app| app.self_entry)
            } else {
                None
            };

            preferred
                .or_else(|| self.apps.iter().find(|app| app.name == metadata.name))
                .map(|app| PreviousInstall {
                    image_path: app.image_path.clone(),
                    desktop_path: app.desktop_path.clone(),
                    exec_arguments: app.exec_arguments.clone(),
                    env_variables: app.env_variables.clone(),
                })
        } else {
            None
        };

        let options = self.integrate_options(conflict, None);
        let config = self.config.clone();
        widgets.progress.set_visible(true);
        widgets.progress.set_text(Some(&t("Integrating...")));

        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut config = config;
            let result =
                install::integrate(&path, &metadata, previous.as_ref(), &options, &mut config)
                    .map(|app| (app, config))
                    .map_err(|error| error.to_string());
            sender.input(Msg::Installed(result));
        });
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

    fn manager_for(&self, path: &Path) -> Result<Box<dyn update::UpdateManager>, String> {
        update::resolve(path, &self.config.update_manager(path))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "No update method was found for this AppImage".to_owned())
    }

    fn files_chosen(
        &mut self,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
        paths: Vec<PathBuf>,
        source: LoadSource,
    ) {
        self.external_open = source == LoadSource::External;
        widgets.drop_revealer.set_reveal_child(false);
        widgets.drop_revealer.set_visible(false);
        let installed = self.apps.iter().map(|app| app.image_path.clone()).collect();
        Self::load_files(
            paths,
            !self.settings.values().block_unsafe_extractor,
            self.paths.temp_dir().join("metadata"),
            installed,
            self.settings.values().appimages_dir.clone(),
            sender,
        );
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the security switch mirrors the block-unsafe-extractor setting"
    )]
    fn load_files(
        paths: Vec<PathBuf>,
        allow_unsafe_extractor: bool,
        work_dir: PathBuf,
        installed: Vec<PathBuf>,
        appimages_dir: PathBuf,
        sender: &ComponentSender<Self>,
    ) {
        if paths.is_empty() {
            return;
        }

        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut loaded = Vec::new();
            let mut errors = Vec::new();

            for path in paths {
                let options = MetadataOptions {
                    allow_unsafe_extractor,
                    work_dir: work_dir.clone(),
                };

                match AppImage::open(&path).and_then(|appimage| appimage.metadata_with(&options)) {
                    Ok(metadata) => {
                        let duplicate = installed
                            .iter()
                            .find(|candidate| same_managed_copy(&path, candidate, &appimages_dir))
                            .cloned();
                        loaded.push(LoadedFile {
                            path,
                            metadata,
                            installed: duplicate,
                        });
                    }
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }

            sender.input(Msg::FilesLoaded(loaded, errors));
        });
    }

    fn open_chooser(root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let dialog = gtk::FileChooserDialog::builder()
            .title(t("Open an AppImage"))
            .transient_for(root)
            .modal(true)
            .action(gtk::FileChooserAction::Open)
            .build();
        let _ = dialog.add_button(&t("Cancel"), gtk::ResponseType::Cancel);
        let _ = dialog.add_button(&t("Open"), gtk::ResponseType::Accept);
        dialog.set_select_multiple(true);

        let sender = sender.clone();
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept {
                let mut paths = Vec::new();
                let files = dialog.files();
                for index in 0..files.n_items() {
                    if let Some(file) = files.item(index).and_downcast::<gio::File>()
                        && let Some(path) = file.path()
                    {
                        paths.push(path);
                    }
                }
                sender.input(Msg::FilesChosen(paths));
            }
            dialog.destroy();
        });

        dialog.present();
    }

    fn show_page(&self, widgets: &Widgets) {
        let (name, title, icon) = match self.page {
            Page::List => ("list", t("Installed applications"), "list-add-symbolic"),
            Page::Details => ("details", t("App details"), "go-previous-symbolic"),
            Page::MultiInstall => (
                "multi",
                t("Move all to the app menu"),
                "go-previous-symbolic",
            ),
            Page::MultiUpdate => ("multi", t("Updating applications"), "go-previous-symbolic"),
        };

        widgets.stack.set_visible_child_name(name);
        widgets.title.set_label(&title);
        widgets
            .title
            .set_visible(!matches!(self.page, Page::Details | Page::MultiUpdate));
        widgets.left_button.set_icon_name(icon);
        widgets
            .left_button
            .set_visible(self.page != Page::MultiUpdate);
        widgets
            .search_button
            .set_visible(matches!(self.page, Page::List | Page::MultiUpdate));
        widgets.search_bar.set_search_mode(false);
        if self.page != Page::MultiUpdate {
            widgets.multi_progress.set_visible(false);
        }
        widgets
            .left_button
            .set_tooltip_text(Some(&if self.page == Page::List {
                t("Open a new AppImage")
            } else {
                t("Back")
            }));

        if self.page == Page::Details {
            // The details rebuild is triggered by the caller.
        }
    }

    fn rebuild_list(&self, widgets: &Widgets) {
        clear_listbox(&widgets.listbox);

        let query = normalize(&self.search);
        let mut visible = 0;

        for app in &self.apps {
            if !query.is_empty() && !normalize(&app.name).contains(&query) {
                continue;
            }

            visible += 1;
            let mut display = app.clone();
            if let Some((path, metadata)) = &self.reloaded
                && path == &app.image_path
            {
                display.name.clone_from(&metadata.name);
                display.description.clone_from(&metadata.comment);
                display.version.clone_from(&metadata.version);
            }
            let row = Self::list_row(
                &display,
                self.updates.contains(&app.image_path),
                &self.paths.metadata_cache_dir(),
            );
            widgets.listbox.append(&row);
        }

        widgets
            .list_stack
            .set_visible_child_name(if visible == 0 { "empty" } else { "list" });
        widgets
            .update_all_button
            .set_visible(!self.updates.is_empty());
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the update badge is a boolean property of the row"
    )]
    fn list_row(app: &InstalledApp, has_update: bool, cache_dir: &Path) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        row.set_widget_name(&app.image_path.to_string_lossy());

        let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        line.set_margin_top(6);
        line.set_margin_bottom(6);
        line.set_margin_start(6);
        line.set_margin_end(6);

        line.append(&util::app_image(app, cache_dir, util::ROW_ICON));

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);

        let name = gtk::Label::new(Some(&app.name));
        name.set_halign(gtk::Align::Start);
        name.add_css_class("heading");
        text.append(&name);

        let mut subtitle = app.version.clone().unwrap_or_default();
        if subtitle.is_empty() {
            subtitle = size(app.size);
        } else {
            subtitle = format!("{subtitle} · {}", size(app.size));
        }

        let subtitle = gtk::Label::new(Some(&subtitle));
        subtitle.set_halign(gtk::Align::Start);
        subtitle.add_css_class("dim-label");
        text.append(&subtitle);

        line.append(&text);

        if has_update {
            line.append(&gtk::Image::from_icon_name(
                "software-update-available-symbolic",
            ));
        }

        row.set_child(Some(&line));
        row
    }

    fn rebuild_details(&self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        while let Some(child) = widgets.details.first_child() {
            widgets.details.remove(&child);
        }

        let Some(item) = &self.details else {
            return;
        };

        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.set_margin_top(18);
        content.set_margin_bottom(24);
        content.set_margin_start(24);
        content.set_margin_end(24);

        match item {
            DetailsItem::Installed(app) => {
                self.build_installed_details(app, &content, sender);
            }
            DetailsItem::Candidate { path, metadata } => {
                Self::build_candidate_details(path, metadata, &content, sender);
            }
        }

        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .child(&content)
            .build();
        widgets.details.append(&clamp);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the details page is built in one place to keep its state local"
    )]
    fn build_installed_details(
        &self,
        app: &InstalledApp,
        content: &gtk::Box,
        sender: &ComponentSender<Self>,
    ) {
        let reloaded = self
            .reloaded
            .as_ref()
            .filter(|(path, _)| path == &app.image_path)
            .map(|(_, metadata)| metadata);

        let (name, description, version, icon) = match reloaded {
            Some(metadata) => (
                metadata.name.clone(),
                metadata.comment.clone(),
                metadata.version.clone(),
                util::candidate_icon(metadata, util::DETAILS_ICON),
            ),
            None => (
                app.name.clone(),
                app.description.clone(),
                app.version.clone(),
                util::app_image(app, &self.paths.metadata_cache_dir(), util::DETAILS_ICON),
            ),
        };

        let minimal = self.minimal_preview()
            || (self.external_open && self.settings.values().preview_minimal_ui);
        let mut pills = Vec::new();
        if !minimal {
            if let Some(version) = version {
                pills.push(version);
            }
            pills.push(size(app.size));
            if let Ok(kind) = detect::image_type(&app.image_path) {
                pills.push(format!("AppImage Type. {}", kind.code()));
            }
        }

        content.append(&details_header(&name, &description, &icon, &pills));

        let terminal = app.terminal;
        let actions = gtk::Box::new(gtk::Orientation::Vertical, 10);
        actions.set_halign(gtk::Align::Center);
        let primary_actions = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        primary_actions.set_halign(gtk::Align::Center);

        let (button_width, button_height) = if minimal {
            (PREVIEW_BUTTON_WIDTH, PREVIEW_BUTTON_HEIGHT)
        } else {
            (ACTION_BUTTON_WIDTH, -1)
        };

        let launch = gtk::Button::with_label(&t("Launch"));
        launch.set_sensitive(!terminal);
        launch.set_size_request(button_width, button_height);
        let sender_launch = sender.clone();
        let path = app.image_path.clone();
        launch.connect_clicked(move |_| sender_launch.input(Msg::Launch(path.clone())));
        primary_actions.append(&launch);

        let remove = gtk::Button::with_label(&t("Remove"));
        remove.add_css_class("destructive-action");
        remove.set_size_request(button_width, button_height);
        let sender_remove = sender.clone();
        let path = app.image_path.clone();
        remove.connect_clicked(move |_| sender_remove.input(Msg::Remove(path.clone())));
        primary_actions.append(&remove);

        actions.append(&primary_actions);

        if !minimal && self.updates.contains(&app.image_path) {
            let update = gtk::Button::with_label(&t("Update"));
            update.add_css_class("suggested-action");
            update.set_size_request(ACTION_BUTTON_WIDTH, -1);
            let sender_update = sender.clone();
            let path = app.image_path.clone();
            update.connect_clicked(move |_| sender_update.input(Msg::StartUpdate(path.clone())));
            actions.append(&update);
        }

        content.append(&actions);

        if minimal {
            return;
        }

        let reload = adw::ActionRow::builder()
            .title(t("Reload metadata"))
            .subtitle(t("Update information like icon, version and description.\nUseful if the app updated itself."))
            .build();
        reload.add_prefix(&gtk::Image::from_icon_name("view-refresh-symbolic"));
        reload.set_activatable(true);
        let sender_reload = sender.clone();
        let path = app.image_path.clone();
        reload.connect_activated(move |_| sender_reload.input(Msg::ReloadMetadata(path.clone())));

        let reload_list = gtk::ListBox::new();
        reload_list.add_css_class("boxed-list");
        reload_list.set_selection_mode(gtk::SelectionMode::None);
        reload_list.append(&reload);
        content.append(&reload_list);

        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);

        let path_row = adw::ActionRow::builder()
            .title(t("Path"))
            .subtitle(app.image_path.to_string_lossy().into_owned())
            .build();
        path_row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
        let open = gtk::Button::from_icon_name("go-next-symbolic");
        open.set_valign(gtk::Align::Center);
        open.set_tooltip_text(Some(&t("Open Folder")));
        let sender_open = sender.clone();
        let path = app.image_path.clone();
        open.connect_clicked(move |_| sender_open.input(Msg::OpenFolder(path.clone())));
        path_row.add_suffix(&open);
        list.append(&path_row);

        let current_arguments = Rc::new(RefCell::new(app.exec_arguments.clone()));

        let args_row = adw::EntryRow::new();
        args_row.set_title(&t("Command line arguments"));
        args_row.set_text(&app.exec_arguments);
        args_row.add_prefix(&gtk::Image::from_icon_name("utilities-terminal-symbolic"));
        let sender_args = sender.clone();
        let path_args = app.image_path.clone();
        let env_args = app.env_variables.clone();
        let arguments_for_row = Rc::clone(&current_arguments);
        let generation = Rc::new(Cell::new(0_u64));
        let generation_for_timer = Rc::clone(&generation);
        let generation_for_destroy = Rc::clone(&generation);
        args_row.connect_destroy(move |_| {
            generation_for_destroy.set(generation_for_destroy.get().wrapping_add(1));
        });
        args_row.connect_changed(move |row| {
            let text = row.text().to_string();
            arguments_for_row.borrow_mut().clone_from(&text);

            let current = generation_for_timer.get().wrapping_add(1);
            generation_for_timer.set(current);
            let sender = sender_args.clone();
            let path = path_args.clone();
            let env = env_args.clone();
            let generation = Rc::clone(&generation_for_timer);
            let arguments = text;
            let _ =
                glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                    if generation.get() == current {
                        sender.input(Msg::SaveArguments {
                            path: path.clone(),
                            env: env.clone(),
                            arguments: arguments.clone(),
                        });
                    }
                });
        });
        list.append(&args_row);

        let website_value = self
            .config
            .app(&app.image_path)
            .get("website")
            .cloned()
            .unwrap_or_default();
        let website_row = adw::EntryRow::new();
        website_row.set_title(&if website_value.is_empty() {
            t("Add a website")
        } else {
            t("Website")
        });
        website_row.set_text(&website_value);
        website_row.add_prefix(&gtk::Image::from_icon_name("web-browser-symbolic"));

        let open_url = gtk::Button::from_icon_name("go-next-symbolic");
        open_url.set_valign(gtk::Align::Center);
        open_url.set_tooltip_text(Some(&t("Open URL")));
        let sender_url = sender.clone();
        let entry_url = website_row.clone();
        open_url.connect_clicked(move |_| {
            let url = entry_url.text().trim().to_owned();
            if url.starts_with("http://") || url.starts_with("https://") {
                sender_url.input(Msg::OpenUrl(url));
            }
        });
        website_row.add_suffix(&open_url);

        let sender_website = sender.clone();
        let path_website = app.image_path.clone();
        website_row.connect_changed(move |row| {
            let text = row.text().trim().to_owned();
            if !text.is_empty() && !text.starts_with("http://") && !text.starts_with("https://") {
                row.remove_css_class("success");
                row.add_css_class("error");
                return;
            }

            row.remove_css_class("error");
            if text.is_empty() {
                row.remove_css_class("success");
            } else {
                row.add_css_class("success");
            }
            sender_website.input(Msg::SaveWebsite {
                path: path_website.clone(),
                website: text,
            });
        });
        list.append(&website_row);

        content.append(&list);
        content.append(&self.update_group(&app.image_path, sender));
        content.append(&Self::env_group(app, sender, current_arguments));
    }

    fn build_candidate_details(
        path: &Path,
        metadata: &AppMetadata,
        content: &gtk::Box,
        sender: &ComponentSender<Self>,
    ) {
        content.append(&details_header(
            &metadata.name,
            "",
            &util::candidate_icon(metadata, util::DETAILS_ICON),
            &[],
        ));

        let actions = gtk::Box::new(gtk::Orientation::Vertical, 10);
        actions.set_halign(gtk::Align::Center);

        let launch = gtk::Button::with_label(&t("Launch"));
        launch.set_size_request(PREVIEW_BUTTON_WIDTH, CANDIDATE_BUTTON_HEIGHT);
        let sender_launch = sender.clone();
        let path_buf = path.to_path_buf();
        launch.connect_clicked(move |_| sender_launch.input(Msg::Launch(path_buf.clone())));
        actions.append(&launch);

        let install = gtk::Button::with_label(&t("Move to the app menu"));
        install.add_css_class("suggested-action");
        install.set_size_request(PREVIEW_BUTTON_WIDTH, CANDIDATE_BUTTON_HEIGHT);
        let sender_install = sender.clone();
        let path_buf = path.to_path_buf();
        install.connect_clicked(move |_| {
            sender_install.input(Msg::InstallCandidate(path_buf.clone()));
        });
        actions.append(&install);

        content.append(&actions);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the update editor keeps its form and save button together"
    )]
    fn update_group(&self, path: &Path, sender: &ComponentSender<Self>) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::new();
        group.set_title(&t("Update management"));

        let config = self.config.update_manager(path);
        let configured = config
            .get("manager")
            .and_then(|name| ManagerKind::from_name(name));
        let selected = self.details_source.or(configured);

        let embedded = selected.is_none()
            && update::resolve(path, &config)
                .ok()
                .flatten()
                .is_some_and(|manager| manager.embedded().is_some());
        if embedded {
            group.set_description(Some(&t(
                "This application includes update information provided by the developer",
            )));
        } else {
            group.set_description(Some(&t("Manage update details for this application")));
        }

        let model = gtk::StringList::new(&[]);
        model.append(&t("Default"));
        for kind in ManagerKind::ALL {
            model.append(kind.label());
        }

        let source_row = adw::ComboRow::builder()
            .title(t("Source"))
            .subtitle(t("Select the source type"))
            .model(&model)
            .build();
        let position = selected.map_or(0, |kind| {
            ManagerKind::ALL
                .iter()
                .position(|candidate| *candidate == kind)
                .map_or(0, |index| u32::try_from(index).unwrap_or(0) + 1)
        });
        source_row.set_selected(position);
        group.add(&source_row);

        if let Some(kind) = selected {
            let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
            form.set_margin_top(6);
            form.set_margin_bottom(6);

            let fields: Rc<RefCell<Vec<(&'static str, FieldWidget)>>> =
                Rc::new(RefCell::new(Vec::new()));

            for field in kind.fields() {
                match field.kind {
                    update::FieldKind::Text => {
                        let row = adw::EntryRow::new();
                        row.set_title(field.label);
                        row.set_text(config.get(field.key).map_or("", String::as_str));
                        form.append(&row);
                        fields
                            .borrow_mut()
                            .push((field.key, FieldWidget::Text(row)));
                    }
                    update::FieldKind::Boolean => {
                        let row = adw::ActionRow::builder().title(field.label).build();
                        let toggle = gtk::Switch::new();
                        toggle.set_valign(gtk::Align::Center);
                        toggle.set_active(matches!(
                            config.get(field.key).map(String::as_str),
                            Some("true" | "yes" | "1")
                        ));
                        row.add_suffix(&toggle);
                        form.append(&row);
                        fields
                            .borrow_mut()
                            .push((field.key, FieldWidget::Boolean(toggle)));
                    }
                    other => {
                        // Gear LeverR only defines text and boolean fields. A
                        // non-exhaustive match keeps this building if the core
                        // grows a new kind, even though this version cannot
                        // render it.
                        tracing::warn!(
                            key = field.key,
                            kind = ?other,
                            "unsupported update source field type"
                        );
                    }
                }
            }

            group.add(&form);

            let apply = gtk::Button::with_label(&t("Save"));
            apply.add_css_class("suggested-action");
            let save_row = adw::ActionRow::builder().title(t("Apply changes")).build();
            save_row.add_suffix(&apply);
            group.add(&save_row);

            let sender = sender.clone();
            let path = path.to_path_buf();
            let fields_for_save = Rc::clone(&fields);
            apply.connect_clicked(move |_| {
                let mut values = BTreeMap::new();
                for (key, widget) in fields_for_save.borrow().iter() {
                    values.insert((*key).to_owned(), widget.value());
                }
                sender.input(Msg::ApplySource {
                    path: path.clone(),
                    kind,
                    values,
                });
            });
        }

        let sender = sender.clone();
        source_row.connect_selected_notify(move |row| {
            let index = row.selected();
            let kind = if index == 0 {
                None
            } else {
                usize::try_from(index - 1)
                    .ok()
                    .and_then(|index| ManagerKind::ALL.get(index).copied())
            };
            sender.input(Msg::SelectSource(kind));
        });

        group
    }

    fn env_group(
        app: &InstalledApp,
        sender: &ComponentSender<Self>,
        arguments: Rc<RefCell<String>>,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::new();
        group.set_title(&t("Environment variables"));
        group.set_description(Some(&t(
            "Add or customize environment for this application",
        )));

        let add = gtk::Button::from_icon_name("list-add-symbolic");
        add.set_valign(gtk::Align::Center);
        let save = gtk::Button::new();
        save.add_css_class("suggested-action");
        save.set_valign(gtk::Align::Center);
        save.set_sensitive(false);
        let content = adw::ButtonContent::new();
        content.set_icon_name("object-select-symbolic");
        content.set_label(&t("Save"));
        save.set_child(Some(&content));

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_valign(gtk::Align::Center);
        header.append(&add);
        header.append(&save);
        group.set_header_suffix(Some(&header));

        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("card");
        card.set_margin_top(10);
        group.add(&card);

        let editor = EnvEditor {
            rows: Rc::new(RefCell::new(Vec::new())),
            container: card,
            save: save.clone(),
            sender: sender.clone(),
            path: app.image_path.clone(),
            arguments,
            next_id: Rc::new(Cell::new(0)),
        };

        for entry in &app.env_variables {
            let (key, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            editor.add(key, value);
        }
        editor.refresh();

        let editor_add = editor.clone();
        add.connect_clicked(move |_| {
            editor_add.add("", "");
            editor_add.refresh();
        });
        let editor_save = editor;
        save.connect_clicked(move |_| editor_save.save());

        group
    }

    fn rebuild_multi(&self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        while let Some(child) = widgets.multi.first_child() {
            widgets.multi.remove(&child);
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 20);
        content.set_margin_top(30);
        content.set_margin_bottom(10);
        content.set_margin_start(20);
        content.set_margin_end(20);

        if self.page == Page::MultiInstall {
            let title = gtk::Label::new(Some(&t("Apps to integrate")));
            title.add_css_class("title-2");
            title.set_halign(gtk::Align::Start);
            content.append(&title);

            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            list.set_selection_mode(gtk::SelectionMode::None);

            for (path, metadata) in &self.pending {
                let row = adw::ActionRow::builder()
                    .title(metadata.name.clone())
                    .subtitle(path.to_string_lossy().into_owned())
                    .build();
                row.add_prefix(&util::candidate_icon(metadata, util::ROW_ICON));
                list.append(&row);
            }
            content.append(&list);

            let all = gtk::Button::with_label(&t("Move all to the app menu"));
            all.add_css_class("suggested-action");
            all.set_halign(gtk::Align::Center);
            let sender = sender.clone();
            all.connect_clicked(move |_| sender.input(Msg::InstallAll));
            content.append(&all);
        } else if self.page == Page::MultiUpdate {
            let cancel = gtk::Button::with_label(&t("Cancel update"));
            cancel.set_halign(gtk::Align::Center);
            let sender_cancel = sender.clone();
            cancel.connect_clicked(move |_| sender_cancel.input(Msg::CancelUpdate));
            content.append(&cancel);

            let list = gtk::ListBox::new();
            list.add_css_class("boxed-list");
            list.set_selection_mode(gtk::SelectionMode::None);

            for app in &self.update_queue {
                let done = self.multi_done.contains(&app.image_path);
                let row = gtk::ListBoxRow::new();
                row.set_activatable(false);
                row.set_opacity(if done { 1.0 } else { 0.5 });

                let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                line.set_margin_top(6);
                line.set_margin_bottom(6);
                line.set_margin_start(6);
                line.set_margin_end(6);
                line.append(&util::app_image(
                    app,
                    &self.paths.metadata_cache_dir(),
                    MULTI_ROW_ICON,
                ));

                let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
                text.set_hexpand(true);
                text.set_valign(gtk::Align::Center);

                let name = gtk::Label::new(Some(&app.name));
                name.set_halign(gtk::Align::Start);
                name.add_css_class("heading");
                name.set_ellipsize(gtk::pango::EllipsizeMode::End);
                name.set_max_width_chars(70);
                text.append(&name);

                if !app.description.trim().is_empty() {
                    let description = gtk::Label::new(Some(&app.description));
                    description.set_halign(gtk::Align::Start);
                    description.add_css_class("dim-label");
                    description.set_ellipsize(gtk::pango::EllipsizeMode::End);
                    description.set_lines(1);
                    description.set_max_width_chars(100);
                    text.append(&description);
                }

                line.append(&text);

                if done {
                    let check = gtk::Image::from_icon_name("object-select-symbolic");
                    check.set_pixel_size(16);
                    check.set_valign(gtk::Align::Center);
                    check.set_margin_end(6);
                    check.add_css_class("success");
                    line.append(&check);
                }

                row.set_child(Some(&line));
                list.append(&row);
            }
            content.append(&list);
        }

        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .child(&content)
            .build();
        widgets.multi.append(&clamp);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the preferences window is a flat list of rows"
    )]
    fn show_preferences(&self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        let window = adw::PreferencesWindow::new();
        window.set_title(Some(&t("Preferences")));
        window.set_transient_for(Some(&widgets.window));
        window.set_modal(true);

        let page = adw::PreferencesPage::new();

        // The original leaves the first group untitled, so keep it that way.
        let general = adw::PreferencesGroup::new();

        let folder_row = adw::ActionRow::builder()
            .title(t("AppImage default location"))
            .subtitle(util::home_relative(
                &self.settings.values().appimages_dir,
                self.paths.home(),
            ))
            .build();
        let choose = gtk::Button::from_icon_name("folder-open-symbolic");
        choose.set_valign(gtk::Align::Center);
        choose.set_tooltip_text(Some(&t("Select a folder")));
        let sender_folder = sender.clone();
        let settings_folder = self.settings.clone();
        let window_folder = window.clone();
        let row_folder = folder_row.clone();
        let home = self.paths.home().to_path_buf();
        choose.connect_clicked(move |_| {
            let dialog = gtk::FileChooserDialog::builder()
                .title(t("Select a folder"))
                .transient_for(&window_folder)
                .modal(true)
                .action(gtk::FileChooserAction::SelectFolder)
                .build();
            let _ = dialog.add_button(&t("Cancel"), gtk::ResponseType::Cancel);
            let _ = dialog.add_button(&t("Select"), gtk::ResponseType::Accept);
            let sender = sender_folder.clone();
            let settings = settings_folder.clone();
            let row = row_folder.clone();
            let home = home.clone();
            dialog.connect_response(move |dialog, response| {
                if response == gtk::ResponseType::Accept
                    && let Some(file) = dialog.file()
                    && let Some(path) = file.path()
                {
                    settings.store_appimages_dir(&path);
                    row.set_subtitle(&util::home_relative(&path, &home));
                    sender.input(Msg::Refresh);
                }
                dialog.destroy();
            });
            dialog.present();
        });
        folder_row.add_suffix(&choose);
        general.add(&folder_row);
        general.add(&self.switch_row(
            &t("Show integrated AppImages outside the default folder"),
            Some(&t(
                "List AppImages that have been integrated into the system menu but are located outside the default folder",
            )),
            keys::MANAGE_OUTSIDE,
            self.settings.values().manage_outside,
            sender,
        ));
        general.add(&self.switch_row(
            &t("Preview new apps in a minimal UI"),
            None,
            keys::PREVIEW_MINIMAL_UI,
            self.settings.values().preview_minimal_ui,
            sender,
        ));
        general.add(&self.switch_row(
            &t("Use safe mechanisms to load metadata"),
            Some(&t(
                "If enabled, the app will try to load an Appimage without using the built-in appimage-extract command.\nDisable only if you trust your sources: malicious apps could execute code when loading the metadata.",
            )),
            keys::BLOCK_UNSAFE_EXTRACTOR,
            self.settings.values().block_unsafe_extractor,
            sender,
        ));

        let updates = adw::PreferencesGroup::new();
        updates.set_title(&t("Updates management"));
        updates.add(&self.switch_row(
            &t("Check updates on system startup"),
            Some(&t(
                "Receive a notification when a new update is detected; updates will not be installed automatically",
            )),
            keys::FETCH_IN_BACKGROUND,
            self.settings.values().fetch_in_background,
            sender,
        ));

        let files = adw::PreferencesGroup::new();
        files.set_title(&t("File management"));
        let move_check = gtk::CheckButton::new();
        move_check.set_active(self.settings.values().move_on_integration);
        let clone_check = gtk::CheckButton::new();
        clone_check.set_group(Some(&move_check));
        clone_check.set_active(!self.settings.values().move_on_integration);

        let settings_move = self.settings.clone();
        let sender_move = sender.clone();
        move_check.connect_toggled(move |check| {
            if check.is_active() {
                settings_move.store_boolean(keys::MOVE_ON_INTEGRATION, true);
                sender_move.input(Msg::Refresh);
            }
        });
        let settings_clone = self.settings.clone();
        let sender_clone = sender.clone();
        clone_check.connect_toggled(move |check| {
            if check.is_active() {
                settings_clone.store_boolean(keys::MOVE_ON_INTEGRATION, false);
                sender_clone.input(Msg::Refresh);
            }
        });

        files.add(&Self::choice_row(
            &t("Move AppImages into the destination folder"),
            &t("Reduce disk usage"),
            &move_check,
        ));
        files.add(&Self::choice_row(
            &t("Clone AppImages into the destination folder"),
            &t("Keep the original file and create a copy in the destination folder"),
            &clone_check,
        ));

        let naming = adw::PreferencesGroup::new();
        naming.set_title(&t("Naming conventions"));
        naming.add(&self.switch_row(
            &t("Use executable name for integrated terminal apps"),
            Some(&t(
                "If enabled, apps that run in the terminal are renamed as their executable.\nYou would need to add the aforementioned folder to your $PATH manually.\n\nFor example, \"golang_x86_64.appimage\" will be saved as \"go\"",
            )),
            keys::EXEC_AS_NAME,
            self.settings.values().exec_name_terminal,
            sender,
        ));

        let debugging = adw::PreferencesGroup::new();
        debugging.set_title(&t("Debugging"));
        debugging.add(&self.switch_row(
            &t("Enable debug logs"),
            Some(&t(
                "Increases log verbosity, occupying more disk space and potentially impacting performance.\nRequires a restart.",
            )),
            keys::DEBUG_LOGS,
            self.settings.values().debug_logs,
            sender,
        ));

        page.add(&general);
        page.add(&updates);
        page.add(&files);
        page.add(&naming);
        page.add(&debugging);
        window.add(&page);
        window.present();
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "a settings switch is a boolean by definition"
    )]
    fn switch_row(
        &self,
        title: &str,
        subtitle: Option<&str>,
        key: &'static str,
        active: bool,
        sender: &ComponentSender<Self>,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder().title(title).build();
        row.set_activatable(true);
        if let Some(subtitle) = subtitle {
            row.set_subtitle(subtitle);
        }

        let toggle = gtk::Switch::new();
        toggle.set_valign(gtk::Align::Center);
        toggle.set_active(active);
        row.add_suffix(&toggle);

        // AdwSwitchRow (libadwaita 1.4) toggles when the row is activated; the
        // equivalent built from ActionRow needs the handler explicitly.
        let toggle_for_row = toggle.clone();
        row.connect_activated(move |_| {
            toggle_for_row.set_active(!toggle_for_row.is_active());
        });

        let settings = self.settings.clone();
        let sender = sender.clone();
        let paths = self.paths.clone();
        let is_autostart = key == keys::FETCH_IN_BACKGROUND;
        let guard = Rc::new(std::cell::Cell::new(false));
        let guard_for_signal = Rc::clone(&guard);

        toggle.connect_active_notify(move |toggle| {
            if guard_for_signal.get() {
                return;
            }

            let active = toggle.is_active();

            if is_autostart {
                match crate::autostart::apply(&paths, active) {
                    Ok(()) => {
                        settings.store_boolean(key, active);
                        sender.input(Msg::Refresh);
                    }
                    Err(error) => {
                        guard_for_signal.set(true);
                        toggle.set_active(!active);
                        guard_for_signal.set(false);
                        sender.input(Msg::Toast(error));
                    }
                }
                return;
            }

            settings.store_boolean(key, active);
            sender.input(Msg::Refresh);
        });

        row
    }

    fn choice_row(title: &str, subtitle: &str, check: &gtk::CheckButton) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        row.set_activatable(true);
        check.set_valign(gtk::Align::Center);
        row.add_prefix(check);

        let check_for_row = check.clone();
        row.connect_activated(move |_| check_for_row.set_active(true));

        row
    }

    fn show_about(parent: &adw::ApplicationWindow) {
        let about = adw::AboutWindow::builder()
            .application_name("Gear LeverR")
            .application_icon("io.github.alarez777.GearLeverR")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name(t("AppImage manager written in Rust"))
            .website(env!("CARGO_PKG_REPOSITORY"))
            .issue_url(concat!(env!("CARGO_PKG_REPOSITORY"), "/issues"))
            .comments(t("<b>About</b>\nAn AppImage manager inspired by Gear Lever, but an independent project: it shares neither its code, its name nor its icon.\n\n<b>Built with</b>\nRust, GTK 4, libadwaita, Relm4, backhand and ureq."))
            .license(t("<b>GPL-3.0-or-later</b>\nFree software: you can redistribute it and/or modify it under the terms of the GNU General Public License, version 3 or later."))
            .build();
        arrange_about_rows(&about);
        about.set_transient_for(Some(parent));
        about.present();
    }

    fn confirm_remove(widgets: &Widgets, path: PathBuf, sender: &ComponentSender<Self>) {
        let dialog = adw::MessageDialog::new(
            Some(&widgets.window),
            Some(&t("Remove application?")),
            Some(&t("Do you really want to remove this app?")),
        );
        dialog.add_response("cancel", &t("Cancel"));
        dialog.add_response("remove", &t("Remove"));
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let sender = sender.clone();
        dialog.connect_response(None, move |dialog: &adw::MessageDialog, response: &str| {
            if response == "remove" {
                sender.input(Msg::RemoveConfirmed(path.clone()));
            }
            dialog.close();
        });
        dialog.present();
    }

    fn confirm_conflict(
        widgets: &Widgets,
        path: PathBuf,
        app_name: &str,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::MessageDialog::new(
            Some(&widgets.window),
            Some(&tf(
                "Conflict with \"{app_name}\"",
                &[("app_name", app_name)],
            )),
            Some(&t(
                "There is already an app with the same name, how do you want to proceed?",
            )),
        );

        // Build the responses by hand so Cancel keeps the filled red
        // destructive style and the three choices stay in this order.
        let buttons = gtk::Box::new(gtk::Orientation::Vertical, 8);
        buttons.set_margin_top(6);

        let replace = gtk::Button::with_label(&t("Replace"));
        replace.add_css_class("suggested-action");
        replace.set_hexpand(true);

        let keep = gtk::Button::with_label(&t("Keep both"));
        keep.set_hexpand(true);

        let cancel = gtk::Button::with_label(&t("Cancel"));
        cancel.add_css_class("destructive-action");
        cancel.set_hexpand(true);

        buttons.append(&replace);
        buttons.append(&keep);
        buttons.append(&cancel);
        dialog.set_extra_child(Some(&buttons));

        let sender_replace = sender.clone();
        let dialog_replace = dialog.clone();
        let replace_path = path.clone();
        replace.connect_clicked(move |_| {
            sender_replace.input(Msg::InstallWithConflict {
                path: replace_path.clone(),
                replace: true,
            });
            dialog_replace.close();
        });

        let sender_keep = sender.clone();
        let dialog_keep = dialog.clone();
        let keep_path = path;
        keep.connect_clicked(move |_| {
            sender_keep.input(Msg::InstallWithConflict {
                path: keep_path.clone(),
                replace: false,
            });
            dialog_keep.close();
        });

        let dialog_cancel = dialog.clone();
        cancel.connect_clicked(move |_| dialog_cancel.close());

        dialog.present();
    }

    fn confirm_self_remove(
        widgets: &Widgets,
        path: PathBuf,
        count: usize,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::MessageDialog::new(
            Some(&widgets.window),
            Some(&t("Remove Gear LeverR?")),
            Some(&t(
                "Gear LeverR will be removed together with its desktop entry and the application will close. Removing only Gear LeverR keeps the other AppImages integrated.",
            )),
        );
        dialog.add_response("cancel", &t("Cancel"));
        dialog.add_response("remove-all", &t("Remove all AppImages"));
        dialog.add_response("remove-self", &t("Remove Gear LeverR"));
        dialog.set_response_appearance("remove-self", adw::ResponseAppearance::Suggested);
        dialog.set_response_appearance("remove-all", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let sender = sender.clone();
        let parent = widgets.window.clone();
        dialog.connect_response(None, move |dialog: &adw::MessageDialog, response: &str| {
            match response {
                "remove-self" => sender.input(Msg::RemoveConfirmed(path.clone())),
                "remove-all" => Self::confirm_remove_all(&parent, count, &sender),
                _ => {}
            }
            dialog.close();
        });
        dialog.present();
    }

    fn confirm_remove_all(
        parent: &adw::ApplicationWindow,
        count: usize,
        sender: &ComponentSender<Self>,
    ) {
        let body = if count == 1 {
            t(
                "Gear LeverR is the only integrated AppImage. It will be removed and the application will close.",
            )
        } else {
            tf(
                "This removes all {count} integrated AppImages, including Gear LeverR, and the application will close.",
                &[("count", &count.to_string())],
            )
        };
        let confirm =
            adw::MessageDialog::new(Some(parent), Some(&t("Remove all AppImages?")), Some(&body));
        confirm.add_response("cancel", &t("Cancel"));
        confirm.add_response("remove-all", &t("Remove all AppImages"));
        confirm.set_response_appearance("remove-all", adw::ResponseAppearance::Destructive);
        confirm.set_default_response(Some("cancel"));
        confirm.set_close_response("cancel");

        let sender = sender.clone();
        confirm.connect_response(None, move |dialog: &adw::MessageDialog, response: &str| {
            if response == "remove-all" {
                sender.input(Msg::RemoveAllConfirmed);
            }
            dialog.close();
        });
        confirm.present();
    }

    fn launch(app: &InstalledApp) {
        let desktop_id = app.desktop_id.clone();
        let image_path = app.image_path.clone();

        // gtk-launch returns immediately once the app is started, so waiting for
        // it does not block anything the user can see.
        std::thread::spawn(move || {
            let launched = launch::command("gtk-launch")
                .arg(&desktop_id)
                .status()
                .is_ok_and(|status| status.success());

            if !launched {
                let _ = launch::command(&image_path).spawn();
            }
        });
    }

    /// Closes Gear LeverR and starts its installed AppImage again.
    ///
    /// The running instance owns the application ID on the session bus, so
    /// starting the image while it is alive would only forward an activation
    /// request and quit. The helper waits for this process to exit first.
    fn restart(widgets: &Widgets, app: &InstalledApp) {
        let _ = Self::restart_into(widgets, &app.image_path, None);
    }

    /// Closes Gear LeverR and starts a candidate AppImage.
    ///
    /// The candidate path is passed back to the new process so it opens the
    /// same install page after the restart.
    fn restart_candidate(widgets: &Widgets, path: &Path) {
        let _ = Self::restart_into(widgets, path, Some(path));
    }

    /// Schedules the restart and quits the current process.
    ///
    /// Returns whether the helper could be spawned; the UI stays open and shows
    /// a toast when it cannot.
    #[must_use]
    fn restart_into(widgets: &Widgets, path: &Path, file: Option<&Path>) -> bool {
        let result = match file {
            Some(file) => launch::relaunch_after_exit_with_file(path, file),
            None => launch::relaunch_after_exit(path),
        };

        if let Err(error) = result {
            tracing::error!(
                %error,
                image = %path.display(),
                "cannot schedule the Gear LeverR restart"
            );
            Self::toast(widgets, &t("Cannot restart Gear LeverR"));
            return false;
        }

        tracing::info!(image = %path.display(), "restarting Gear LeverR");
        relm4::main_application().quit();
        true
    }

    fn launch_file(path: &Path) {
        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            if permissions.mode() & 0o111 == 0 {
                permissions.set_mode(0o755);
                let _ = fs::set_permissions(path, permissions);
            }
        }

        let _ = launch::command(path).spawn();
    }

    fn open_uri(uri: &str) {
        let _ = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>);
    }

    fn minimal_ui(widgets: &Widgets, preview: Preview) {
        widgets.left_button.set_visible(false);
        widgets.menu_button.set_visible(false);
        widgets.search_button.set_visible(false);
        widgets.title.set_visible(false);
        widgets.window.set_resizable(false);

        let (width, height) = match preview {
            Preview::Candidate => (600, 420),
            Preview::Installed => (600, 360),
        };
        widgets.window.set_default_size(width, height);
    }

    fn show_progress(widgets: &Widgets) {
        widgets.progress.set_visible(true);
        widgets.cancel_button.set_visible(true);
        widgets.cancel_button.set_sensitive(true);
        widgets.cancel_button.set_label(&t("Cancel update"));
    }

    fn hide_progress(widgets: &Widgets) {
        widgets.progress.set_visible(false);
        widgets.cancel_button.set_visible(false);
        widgets.cancel_button.set_sensitive(true);
        widgets.cancel_button.set_label(&t("Cancel update"));
    }

    fn toast(widgets: &Widgets, message: &str) {
        widgets.toast.add_toast(adw::Toast::new(message));
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the shell is assembled once, in one place"
    )]
    fn build_shell(root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) -> Widgets {
        let header = adw::HeaderBar::new();

        let title = gtk::Label::new(Some(&t("Installed applications")));
        header.set_title_widget(Some(&title));

        let left_button = gtk::Button::from_icon_name("list-add-symbolic");
        left_button.set_tooltip_text(Some(&t("Open a new AppImage")));
        let sender_left = sender.clone();
        left_button.connect_clicked(move |_| sender_left.input(Msg::LeftButton));

        let search_button = gtk::Button::from_icon_name("system-search-symbolic");
        search_button.set_tooltip_text(Some(&t("Search")));
        let sender_search = sender.clone();
        search_button.connect_clicked(move |_| sender_search.input(Msg::ToggleSearch));

        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(t("Menu"))
            .build();

        let menu = gio::Menu::new();
        menu.append(Some(&t("_Preferences")), Some("win.preferences"));
        menu.append(Some(&t("_Open Log File")), Some("win.log"));
        menu.append(Some(&t("_About Gear LeverR")), Some("win.about"));
        menu.append(Some(&t("Quit")), Some("win.quit"));
        menu_button.set_menu_model(Some(&menu));

        header.pack_start(&left_button);
        header.pack_end(&menu_button);
        header.pack_end(&search_button);

        let preferences = gio::SimpleAction::new("preferences", None);
        let sender_preferences = sender.clone();
        preferences.connect_activate(move |_, _| sender_preferences.input(Msg::ShowPreferences));
        root.add_action(&preferences);

        let about = gio::SimpleAction::new("about", None);
        let sender_about = sender.clone();
        about.connect_activate(move |_, _| sender_about.input(Msg::ShowAbout));
        root.add_action(&about);

        let log = gio::SimpleAction::new("log", None);
        let sender_log = sender.clone();
        log.connect_activate(move |_, _| sender_log.input(Msg::ShowLog));
        root.add_action(&log);

        let quit = gio::SimpleAction::new("quit", None);
        let window = root.clone();
        quit.connect_activate(move |_, _| window.close());
        root.add_action(&quit);

        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some(&t("Filter installed applications")));
        let search_bar = gtk::SearchBar::new();
        search_bar.set_child(Some(&search_entry));
        search_bar.connect_entry(&search_entry);
        search_bar.set_key_capture_widget(Some(root));
        search_bar.set_search_mode(false);
        let sender_filter = sender.clone();
        search_entry.connect_search_changed(move |entry| {
            sender_filter.input(Msg::SearchChanged(entry.text().to_string()));
        });

        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let list_title = gtk::Label::new(Some(&t("Installed applications")));
        list_title.add_css_class("title-2");
        list_title.set_halign(gtk::Align::Start);
        list_title.set_hexpand(true);
        title_row.append(&list_title);

        let updates_button = gtk::Button::with_label(&t("Check for updates"));
        let sender_updates = sender.clone();
        updates_button.connect_clicked(move |_| sender_updates.input(Msg::CheckUpdates));
        title_row.append(&updates_button);

        let update_all_button = gtk::Button::with_label(&t("Update all"));
        update_all_button.add_css_class("suggested-action");
        update_all_button.set_visible(false);
        let sender_update_all = sender.clone();
        update_all_button.connect_clicked(move |_| sender_update_all.input(Msg::StartAllUpdates));
        title_row.append(&update_all_button);

        let listbox = gtk::ListBox::new();
        listbox.add_css_class("boxed-list");
        listbox.set_selection_mode(gtk::SelectionMode::None);
        let sender_row = sender.clone();
        listbox.connect_row_activated(move |_, row| {
            sender_row.input(Msg::OpenPath(PathBuf::from(row.widget_name())));
        });

        let list_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&listbox)
            .build();

        let empty = adw::StatusPage::builder()
            .icon_name("system-software-install-symbolic")
            .title(t("Get started"))
            .description(t(
                "Drag and drop an AppImage here or click on the \"+\" icon.",
            ))
            .build();
        let empty_buttons = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let preferences = gtk::Button::with_label(&t("Open Preferences"));
        preferences.add_css_class("pill");
        let sender_prefs = sender.clone();
        preferences.connect_clicked(move |_| sender_prefs.input(Msg::ShowPreferences));
        empty_buttons.append(&preferences);
        empty.set_child(Some(&empty_buttons));

        let list_stack = gtk::Stack::new();
        list_stack.add_named(&list_scroll, Some("list"));
        list_stack.add_named(&empty, Some("empty"));

        let list_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        list_page.set_margin_top(18);
        list_page.set_margin_bottom(18);
        list_page.set_margin_start(18);
        list_page.set_margin_end(18);
        list_page.append(&title_row);
        list_page.append(&list_stack);

        let list_clamp = adw::Clamp::builder()
            .maximum_size(640)
            .child(&list_page)
            .build();
        let list_view = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&list_clamp)
            .build();

        let details_scroll = gtk::ScrolledWindow::builder().vexpand(true).build();
        let details = gtk::Box::new(gtk::Orientation::Vertical, 0);
        details_scroll.set_child(Some(&details));

        let multi_scroll = gtk::ScrolledWindow::builder().vexpand(true).build();
        let multi = gtk::Box::new(gtk::Orientation::Vertical, 0);
        multi_scroll.set_child(Some(&multi));

        let multi_progress = gtk::ProgressBar::new();
        multi_progress.set_visible(false);
        multi_progress.set_show_text(false);
        multi_progress.set_height_request(4);
        multi_progress.set_valign(gtk::Align::Start);
        multi_progress.set_hexpand(true);
        multi_progress.add_css_class("multi-progress");

        let multi_overlay = gtk::Overlay::new();
        multi_overlay.set_child(Some(&multi_scroll));
        multi_overlay.add_overlay(&multi_progress);

        let stack = gtk::Stack::new();
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_vexpand(true);
        stack.add_named(&list_view, Some("list"));
        stack.add_named(&details_scroll, Some("details"));
        stack.add_named(&multi_overlay, Some("multi"));

        let progress = gtk::ProgressBar::new();
        progress.set_visible(false);
        progress.set_show_text(true);
        progress.set_hexpand(true);

        let cancel_button = gtk::Button::with_label(&t("Cancel update"));
        cancel_button.set_visible(false);
        cancel_button.add_css_class("destructive-action");
        let sender_cancel = sender.clone();
        cancel_button.connect_clicked(move |_| sender_cancel.input(Msg::CancelUpdate));

        let progress_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        progress_row.set_margin_start(12);
        progress_row.set_margin_end(12);
        progress_row.set_margin_bottom(6);
        progress_row.append(&progress);
        progress_row.append(&cancel_button);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&header);
        content.append(&search_bar);
        content.append(&stack);
        content.append(&progress_row);

        let drop_page = adw::StatusPage::builder()
            .icon_name("document-send-symbolic")
            .title(t("Drop your file here"))
            .description(t("Release to open the AppImage"))
            .build();
        let drop_revealer = gtk::Revealer::new();
        drop_revealer.set_child(Some(&drop_page));
        drop_revealer.set_transition_type(gtk::RevealerTransitionType::Crossfade);
        drop_revealer.set_reveal_child(false);
        // An overlay child that stays visible swallows every pointer event,
        // even when its child is not revealed. Keep it hidden and never let
        // it take input; the drop target lives on the content below.
        drop_revealer.set_visible(false);
        drop_revealer.set_can_target(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&content));
        overlay.add_overlay(&drop_revealer);

        let toast = adw::ToastOverlay::new();
        toast.set_child(Some(&overlay));
        root.set_content(Some(&toast));

        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let sender_drop = sender.clone();
        drop.connect_drop(move |_, value, _, _| {
            let Ok(files) = value.get::<gdk::FileList>() else {
                return false;
            };
            let paths: Vec<PathBuf> = files
                .files()
                .iter()
                .filter_map(gio::prelude::FileExt::path)
                .collect();
            sender_drop.input(Msg::FilesChosen(paths));
            true
        });
        let sender_enter = sender.clone();
        drop.connect_enter(move |_, _, _| {
            sender_enter.input(Msg::DragActive(true));
            gdk::DragAction::COPY
        });
        let sender_leave = sender.clone();
        drop.connect_leave(move |_| {
            sender_leave.input(Msg::DragActive(false));
        });
        content.add_controller(drop);

        let paths = Paths::from_env().unwrap_or_else(|_| Paths::with_home(glib::home_dir()));
        let temp = paths.temp_dir();
        let settings = Settings::load(&paths);
        let window_close = root.clone();
        root.connect_close_request(move |window| {
            settings.store_boolean(keys::IS_MAXIMIZED, window.is_maximized());
            let _ = fs::remove_dir_all(&temp);
            glib::Propagation::Proceed
        });

        Widgets {
            window: window_close,
            title,
            left_button,
            search_button,
            menu_button,
            search_bar,
            stack,
            toast,
            listbox,
            list_stack,
            updates_button,
            update_all_button,
            details,
            multi,
            multi_progress,
            progress,
            cancel_button,
            drop_revealer,
            welcome: None,
        }
    }
}

#[derive(Clone)]
struct EnvEditor {
    rows: Rc<RefCell<Vec<EnvRow>>>,
    container: gtk::Box,
    save: gtk::Button,
    sender: ComponentSender<AppModel>,
    path: PathBuf,
    arguments: Rc<RefCell<String>>,
    next_id: Rc<Cell<u64>>,
}

struct EnvRow {
    id: u64,
    key: gtk::Entry,
    value: gtk::Entry,
}

impl EnvEditor {
    fn add(&self, key: &str, value: &str) {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));

        let key_entry = gtk::Entry::new();
        key_entry.set_placeholder_text(Some(&t("Key")));
        key_entry.set_text(key);
        key_entry.set_hexpand(true);

        let value_entry = gtk::Entry::new();
        value_entry.set_placeholder_text(Some(&t("Value")));
        value_entry.set_text(value);
        value_entry.set_hexpand(true);
        value_entry.set_sensitive(!key.is_empty());

        let delete = gtk::Button::from_icon_name("user-trash-symbolic");
        delete.add_css_class("destructive-action");
        delete.set_valign(gtk::Align::Center);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(12);
        row.set_margin_end(12);
        row.append(&key_entry);
        row.append(&gtk::Label::new(Some("=")));
        row.append(&value_entry);
        row.append(&delete);

        self.container.append(&row);
        self.rows.borrow_mut().push(EnvRow {
            id,
            key: key_entry.clone(),
            value: value_entry.clone(),
        });

        let editor = self.clone();
        key_entry.connect_changed(move |_| editor.refresh());
        let editor = self.clone();
        value_entry.connect_changed(move |_| editor.refresh());
        let editor = self.clone();
        let row_for_delete = row;
        delete.connect_clicked(move |_| {
            {
                let mut rows = editor.rows.borrow_mut();
                rows.retain(|entry| entry.id != id);
            }
            editor.container.remove(&row_for_delete);
            editor.refresh();
            editor.save();
        });
    }

    fn refresh(&self) {
        let rows = self.rows.borrow();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for row in rows.iter() {
            let key = row.key.text().trim().to_owned();
            if !key.is_empty() {
                *counts.entry(key).or_default() += 1;
            }
        }

        let mut valid = !rows.is_empty();
        for row in rows.iter() {
            let key = row.key.text().trim().to_owned();
            let count = counts.get(&key).copied().unwrap_or(0);
            let unique = !key.is_empty() && count == 1;

            if !key.is_empty() && count > 1 {
                row.key.add_css_class("error");
            } else {
                row.key.remove_css_class("error");
            }
            row.value.set_sensitive(unique);
            if !unique {
                valid = false;
            }
        }

        self.save.set_sensitive(valid);
    }

    fn save(&self) {
        let rows = self.rows.borrow();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for row in rows.iter() {
            let key = row.key.text().trim().to_owned();
            if !key.is_empty() {
                *counts.entry(key).or_default() += 1;
            }
        }

        let mut env = Vec::new();
        for row in rows.iter() {
            let key = row.key.text().trim().to_owned();
            if key.is_empty() || counts.get(&key).copied().unwrap_or(0) != 1 {
                continue;
            }
            let value = row.value.text().trim().to_owned();
            env.push(format!("{key}={}", gearlever_core::args::quote(&value)));
        }

        let arguments = self.arguments.borrow().clone();
        self.sender.input(Msg::SaveExec {
            path: self.path.clone(),
            env,
            arguments,
        });
    }
}

#[derive(Debug)]
enum FieldWidget {
    Text(adw::EntryRow),
    Boolean(gtk::Switch),
}

impl FieldWidget {
    fn value(&self) -> String {
        match self {
            Self::Text(row) => row.text().to_string(),
            Self::Boolean(row) => {
                if row.is_active() {
                    "true".to_owned()
                } else {
                    "false".to_owned()
                }
            }
        }
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "progress fractions do not need exact precision"
)]
fn fraction(done: usize, total: usize) -> f64 {
    done as f64 / total as f64
}

/// Width shared by the primary action buttons.
const ACTION_BUTTON_WIDTH: i32 = 200;

/// Icon size used by the rows of the multi-update page.
const MULTI_ROW_ICON: i32 = 45;

/// Width and height of the buttons in the small preview windows.
const PREVIEW_BUTTON_WIDTH: i32 = 240;
const PREVIEW_BUTTON_HEIGHT: i32 = 36;
/// Height of Launch and Move in the new-AppImage preview.
const CANDIDATE_BUTTON_HEIGHT: i32 = 54;

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Returns whether two directories are the same one.
fn same_directory(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Returns whether `candidate` is the installed copy of the opened `path`.
///
/// The exact path always counts. Content comparison only applies when both
/// files live in the managed AppImages folder, so a build opened from
/// `Downloads` or `dist` stays installable even when its bytes match an
/// installed AppImage.
fn same_managed_copy(path: &Path, candidate: &Path, appimages_dir: &Path) -> bool {
    if same_file(path, candidate) {
        return true;
    }

    let path_is_managed = path
        .parent()
        .is_some_and(|parent| same_directory(parent, appimages_dir));
    let candidate_is_managed = candidate
        .parent()
        .is_some_and(|parent| same_directory(parent, appimages_dir));

    path_is_managed && candidate_is_managed && same_content(path, candidate)
}

/// Returns whether launching `app` would start Gear LeverR itself.
fn is_self_launch(app: &InstalledApp) -> bool {
    app.self_entry || running_appimage().is_some_and(|image| same_file(&image, &app.image_path))
}

/// Returns the AppImage this process was started from, when there is one.
fn running_appimage() -> Option<PathBuf> {
    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

/// Lays the about window out like the original Gear Lever.
///
/// `AdwAboutWindow` builds its rows and groups in a fixed order and offers no
/// API to rearrange them. The original shows the issue link alone and then one
/// group with "Credits" and "Legal"; here the same group holds "Details" and
/// "Legal". Rows are located by their action target, and anything that does not
/// match leaves the window as libadwaita built it.
fn arrange_about_rows(about: &adw::AboutWindow) {
    let root: &gtk::Widget = about.upcast_ref();
    let (Some(details), Some(legal), Some(issue)) = (
        about_row(root, "details"),
        about_row(root, "legal"),
        about_row(root, "issue-url"),
    ) else {
        return;
    };
    let (Some(legal_group), Some(support_group)) = (
        ancestor_group(legal.upcast_ref()),
        ancestor_group(issue.upcast_ref()),
    ) else {
        return;
    };
    let Some(list) = details
        .parent()
        .and_then(|parent| parent.downcast::<gtk::ListBox>().ok())
    else {
        return;
    };

    // One group with "Details" and "Legal", then drop the group left empty.
    legal.unparent();
    list.insert(&legal, details.index() + 1);
    if !has_visible_rows(&legal_group) {
        legal_group.set_visible(false);
    }

    // The issue link goes first, above that group.
    if let Some(container) = support_group
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Box>().ok())
    {
        container.reorder_child_after(&support_group, gtk::Widget::NONE);
    }
}

/// Finds the about row with the given action target.
fn about_row(root: &gtk::Widget, target: &str) -> Option<adw::ActionRow> {
    if let Ok(row) = root.clone().downcast::<adw::ActionRow>() {
        let matches = row
            .property::<Option<glib::Variant>>("action-target")
            .and_then(|value| value.get::<String>())
            .as_deref()
            == Some(target);
        if matches {
            return Some(row);
        }
    }

    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(found) = about_row(&widget, target) {
            return Some(found);
        }
        child = widget.next_sibling();
    }

    None
}

/// Walks up from a widget to the preferences group that holds it.
fn ancestor_group(widget: &gtk::Widget) -> Option<adw::PreferencesGroup> {
    let mut current = widget.parent();
    while let Some(parent) = current {
        if let Ok(group) = parent.clone().downcast::<adw::PreferencesGroup>() {
            return Some(group);
        }
        current = parent.parent();
    }

    None
}

/// Returns whether the group still has a row of its own on screen.
fn has_visible_rows(group: &adw::PreferencesGroup) -> bool {
    let mut pending = vec![group.clone().upcast::<gtk::Widget>()];
    while let Some(widget) = pending.pop() {
        if widget.is::<adw::ActionRow>() && widget.is_visible() {
            return true;
        }

        let mut child = widget.first_child();
        while let Some(next) = child {
            pending.push(next.clone());
            child = next.next_sibling();
        }
    }

    false
}

fn clear_listbox(listbox: &gtk::ListBox) {
    while let Some(child) = listbox.first_child() {
        listbox.remove(&child);
    }
}

fn details_header(
    name: &str,
    description: &str,
    icon: &gtk::Image,
    pills: &[String],
) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
    header.set_halign(gtk::Align::Center);

    header.append(icon);

    let title = gtk::Label::new(Some(name));
    title.add_css_class("title-1");
    title.set_justify(gtk::Justification::Center);
    title.set_wrap(true);
    header.append(&title);

    if !description.trim().is_empty() {
        let subtitle = gtk::Label::new(Some(description));
        subtitle.add_css_class("dim-label");
        subtitle.add_css_class("subtitle");
        subtitle.set_justify(gtk::Justification::Center);
        subtitle.set_wrap(true);
        subtitle.set_lines(2);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle.set_max_width_chars(50);
        header.append(&subtitle);
    }

    let pills_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    pills_box.set_halign(gtk::Align::Center);
    pills_box.set_margin_top(10);
    pills_box.set_margin_bottom(4);
    for value in pills {
        pills_box.append(&pill(value));
    }
    header.append(&pills_box);

    header.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appimage(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("write the AppImage");
    }

    #[test]
    fn detects_a_copy_inside_the_managed_folder() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let managed = directory.path().join("AppImages");
        fs::create_dir_all(&managed).expect("managed directory");
        let installed = managed.join("Installed.AppImage");
        let copy = managed.join("Copy.AppImage");
        appimage(&installed, b"same bytes");
        appimage(&copy, b"same bytes");

        assert!(
            same_managed_copy(&copy, &installed, &managed),
            "a copy in the managed folder is the installed AppImage"
        );
    }

    #[test]
    fn does_not_match_a_copy_outside_the_managed_folder() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let managed = directory.path().join("AppImages");
        let downloads = directory.path().join("Downloads");
        fs::create_dir_all(&managed).expect("managed directory");
        fs::create_dir_all(&downloads).expect("downloads directory");
        let installed = managed.join("Installed.AppImage");
        let copy = downloads.join("Copy.AppImage");
        appimage(&installed, b"same bytes");
        appimage(&copy, b"same bytes");

        assert!(
            !same_managed_copy(&copy, &installed, &managed),
            "a copy outside the managed folder stays installable"
        );
    }

    #[test]
    fn keeps_an_exact_path_outside_the_managed_folder() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let managed = directory.path().join("AppImages");
        let downloads = directory.path().join("Downloads");
        fs::create_dir_all(&managed).expect("managed directory");
        fs::create_dir_all(&downloads).expect("downloads directory");
        let outside = downloads.join("Installed.AppImage");
        appimage(&outside, b"same bytes");

        assert!(
            same_managed_copy(&outside, &outside, &managed),
            "the exact path always counts as installed"
        );
    }

    #[test]
    fn ignores_an_installed_copy_outside_the_managed_folder() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let managed = directory.path().join("AppImages");
        let downloads = directory.path().join("Downloads");
        fs::create_dir_all(&managed).expect("managed directory");
        fs::create_dir_all(&downloads).expect("downloads directory");
        let installed = downloads.join("Installed.AppImage");
        let opened = managed.join("Opened.AppImage");
        appimage(&installed, b"same bytes");
        appimage(&opened, b"same bytes");

        assert!(
            !same_managed_copy(&opened, &installed, &managed),
            "content only counts when both files are managed"
        );
    }
}
