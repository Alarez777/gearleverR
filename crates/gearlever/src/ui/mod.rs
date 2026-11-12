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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use relm4::adw::prelude::*;
use relm4::gtk::{gdk, gio, glib};
use relm4::prelude::*;
use relm4::{adw, gtk};

use gearlever_core::Error as CoreError;
use gearlever_core::appimage::{
    AppImage, AppMetadata, ElfArch, MetadataOptions, PayloadKind, detect, is_running,
};
use gearlever_core::cache::MetadataCache;
use gearlever_core::config::Config;
use gearlever_core::error::RateLimitInfo;
use gearlever_core::format::size;
use gearlever_core::hash::HashCache;
use gearlever_core::install::{self, Conflict, IntegrateOptions, PreviousInstall};
use gearlever_core::paths::Paths;
use gearlever_core::scan::{InstalledApp, ScanOptions, scan};
use gearlever_core::self_install;
use gearlever_core::system::{self, FuseComponent, FuseReport, RunningMode};
use gearlever_core::uninstall::{Removal, uninstall};
use gearlever_core::update::github::TokenCheck;
use gearlever_core::update::{self, ManagerKind, UpdateOptions, UpdateStatus};

use crate::i18n::{t, tf};
use crate::launch;
use crate::rate_limit::{self, TokenState};
use crate::settings::{APP_ID, MAX_PARALLEL_CHECKS, MAX_PARALLEL_DOWNLOADS, Settings, keys};
use crate::token_store::{TokenLookup, TokenStatus, TokenStore, valid_token};
use util::{normalize, pill};

/// Time budget for the sequential retry pass after the parallel checks.
const RETRY_BUDGET: Duration = update::DEFAULT_RETRY_BUDGET;

/// How long the green check stays visible before the row moves to the installed list.
const SINGLE_UPDATE_CHECK_DURATION: Duration = Duration::from_secs(1);

/// How long an error toast stays visible, in seconds.
const ERROR_TOAST_TIMEOUT_SECS: u32 = 10;

/// Application name used by the main window and its header.
const APP_NAME: &str = "Gear LeverR";

/// Documentation for installing FUSE on Linux distributions.
const FUSE_DOCS_URL: &str = "https://github.com/AppImage/AppImageKit/wiki/FUSE";

/// Whether a system capability was detected in Preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemStatus {
    Detected,
    Missing,
}

/// Starts the graphical application.
pub(crate) fn run(arguments: Vec<String>) {
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

impl Page {
    /// Returns whether the header shows the left button.
    ///
    /// The button opens a file on the list and goes back everywhere else. A
    /// one-shot preview hides it because it has no list behind it; an external
    /// open into a running window keeps it so the user can return to the list.
    /// `MultiUpdate` starts without it; `show_page` reveals it once the batch
    /// finishes so the user can leave the results at their own pace.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the page only needs the preview-window flag to choose its button"
    )]
    fn shows_left_button(self, preview_window: bool) -> bool {
        match self {
            Self::List => true,
            Self::Details | Self::MultiInstall => !preview_window,
            Self::MultiUpdate => false,
        }
    }
}

/// Where the files came from: the external open signal or an in-app chooser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadSource {
    Chosen,
    External,
}

/// What happens to the window after starting an installed application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowAfterLaunch {
    StayOpen,
    Close,
}

#[expect(
    clippy::large_enum_variant,
    reason = "the installed path is the common case and cloning is cheap enough"
)]
#[derive(Debug, Clone)]
enum DetailsItem {
    Installed(InstalledApp),
    Candidate(Box<CandidateFile>),
}

/// An AppImage that is not integrated yet, together with the data read from it.
#[derive(Debug, Clone)]
struct CandidateFile {
    path: PathBuf,
    metadata: AppMetadata,
    arch: ElfArch,
}

/// One AppImage read from the command line, the chooser or a drop.
#[derive(Debug)]
struct LoadedFile {
    candidate: CandidateFile,
    installed: Option<PathBuf>,
}

/// One AppImage that could not be read, with the message shown to the user.
#[derive(Debug)]
struct LoadFailure {
    path: PathBuf,
    message: String,
    kind: LoadFailureKind,
}

/// Whether a load failure needs the ISO9660 dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadFailureKind {
    Iso9660Extractor,
    Other,
}

/// Why a selected AppImage will not be integrated in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IgnoredReason {
    ForeignArchitecture,
    AlreadyInstalled(PathBuf),
}

/// An AppImage shown on the batch page that cannot be integrated.
#[derive(Debug, Clone)]
struct IgnoredCandidate {
    candidate: CandidateFile,
    reason: IgnoredReason,
}

/// What to do with every name conflict in a batch integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchConflictPolicy {
    ReplaceAll,
    KeepAll,
    AskEach,
}

/// What to do with one AppImage that conflicts with an installed app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchConflictDecision {
    Replace,
    KeepBoth,
    Skip,
}

/// Which kind of conflict dialog is being shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictDialogScope {
    Single,
    Batch,
}

/// One integration of a batch, with its conflict already resolved.
#[derive(Debug)]
struct InstallJob {
    path: PathBuf,
    metadata: AppMetadata,
    conflict: Conflict,
    previous: Option<PreviousInstall>,
}

/// Outcome of integrating a batch.
#[derive(Debug)]
struct BatchInstallResult {
    config: Config,
    integrated: Vec<PathBuf>,
    failed: Vec<PathBuf>,
}

/// Widgets of one row on the multi-update page, updated without rebuilding it.
struct MultiUpdateRow {
    row: gtk::ListBoxRow,
    status: gtk::Stack,
    ring: util::CircularProgress,
    error: gtk::Label,
}

/// Stage of an update started from a single application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SingleUpdateStage {
    Running,
    Done,
}

/// State of the update started from a single application.
#[derive(Debug)]
struct SingleUpdate {
    path: PathBuf,
    fraction: f64,
    stage: SingleUpdateStage,
}

/// Widgets that show the progress of a single update in the visible view.
struct SingleUpdateRow {
    status: gtk::Stack,
    ring: util::CircularProgress,
}

/// Action shown at the end of a row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    Open,
    Update,
}

/// One row of the updates or installed lists.
struct ListRow {
    row: gtk::ListBoxRow,
    single: Option<SingleUpdateRow>,
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
    FilesLoaded(Vec<LoadedFile>, Vec<LoadFailure>),
    ArchDecision(bool),
    DragActive(bool),
    CheckUpdates,
    CheckAppUpdates(PathBuf),
    UpdateCheckStarted(usize),
    UpdateChecked(PathBuf, bool, Option<Arc<dyn update::UpdateManager>>),
    UpdateRetryStarted,
    CheckDone,
    GithubTokenTested(TokenCheck),
    GithubTokenInvalid,
    GithubRateLimited {
        info: RateLimitInfo,
        token_state: TokenState,
    },
    InstallCandidate(PathBuf),
    InstallWithConflict {
        path: PathBuf,
        replace: bool,
    },
    InstallAll,
    BatchPolicy(BatchConflictPolicy),
    BatchConflictDecision {
        path: PathBuf,
        decision: BatchConflictDecision,
    },
    BatchConflictCancelled,
    Installed(Result<(InstalledApp, Config), String>),
    MultiInstalled(Result<BatchInstallResult, String>),
    Remove(PathBuf),
    RemoveConfirmed(PathBuf),
    Removed(Result<(PathBuf, Config), String>),
    RemoveAllConfirmed,
    AllRemoved {
        config: Config,
        errors: Vec<String>,
    },
    Launch(PathBuf),
    CloseAfterLaunch,
    StartUpdate(PathBuf),
    StartAllUpdates,
    Progress(f64, String),
    SingleProgress(f64),
    CancelUpdate,
    UpdateCancelled,
    Updated(Result<(InstalledApp, Config), String>),
    SingleUpdateFinished(PathBuf),
    MultiAppStarted(PathBuf),
    MultiAppUpdated(PathBuf),
    MultiAppCancelled(PathBuf),
    MultiAppFailed {
        path: PathBuf,
        error: String,
    },
    MultiProgress {
        path: PathBuf,
        fraction: f64,
        overall: f64,
    },
    MultiUpdated(Result<Config, String>, bool),
    SaveExec {
        path: PathBuf,
        env: Vec<String>,
        disabled_env: Vec<String>,
        arguments: String,
    },
    ForgetEnvironment(String),
    SaveArguments {
        path: PathBuf,
        env: Vec<String>,
        arguments: String,
    },
    SaveWebsite {
        path: PathBuf,
        website: String,
    },
    SetUpdatesEnabled {
        path: PathBuf,
        enabled: bool,
    },
    SetParallelDownloads(usize),
    SetParallelChecks(usize),
    UiScaleChanged(u32),
    RestartApp,
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
    ExtractionNotice,
    Welcome,
    WelcomeRecheckDependencies,
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
    applied_ui_scale: u32,
    config: Config,
    apps: Vec<InstalledApp>,
    page: Page,
    details: Option<DetailsItem>,
    details_source: Option<ManagerKind>,
    search: String,
    updates: HashSet<PathBuf>,
    managers: HashMap<PathBuf, Arc<dyn update::UpdateManager>>,
    checking: bool,
    check_total: usize,
    check_done: usize,
    pending: Vec<CandidateFile>,
    pending_arch: Option<Vec<LoadedFile>>,
    multi_ignored: Vec<IgnoredCandidate>,
    batch_conflicts: Vec<PathBuf>,
    batch_decisions: HashMap<PathBuf, Conflict>,
    batch_skipped: HashSet<PathBuf>,
    batch_ask_index: usize,
    install_running: bool,
    multi_done: HashSet<PathBuf>,
    multi_active: HashSet<PathBuf>,
    multi_fractions: HashMap<PathBuf, f64>,
    multi_cancelled: HashSet<PathBuf>,
    multi_failed: HashMap<PathBuf, String>,
    multi_rows: HashMap<PathBuf, MultiUpdateRow>,
    multi_finished: bool,
    single_update: Option<SingleUpdate>,
    single_row: Option<SingleUpdateRow>,
    update_queue: Vec<InstalledApp>,
    opened_from_file: bool,
    /// Whether the window was created to preview an externally opened AppImage.
    ///
    /// This is a one-shot preview with no list behind it. An external open into
    /// an already running window keeps the normal list and must not set it.
    preview_window: bool,
    external_open: bool,
    reloaded: Option<(PathBuf, AppMetadata)>,
    cancel_token: Option<Arc<AtomicBool>>,
    removing_self: bool,
}

struct Widgets {
    window: adw::ApplicationWindow,
    header: adw::HeaderBar,
    title: gtk::Label,
    left_button: gtk::Button,
    search_button: gtk::Button,
    menu_button: gtk::MenuButton,
    search_bar: gtk::SearchBar,
    stack: gtk::Stack,
    toast: adw::ToastOverlay,
    updates_section: gtk::Box,
    updates_list: gtk::ListBox,
    installed_section: gtk::Box,
    installed_header: gtk::Label,
    installed_list: gtk::ListBox,
    list_title: gtk::Label,
    list_stack: gtk::Stack,
    updates_button: gtk::Button,
    updates_progress: gtk::ProgressBar,
    updates_generation: Rc<Cell<u64>>,
    update_all_button: gtk::Button,
    details: gtk::Box,
    multi: gtk::Box,
    multi_progress: gtk::ProgressBar,
    progress: gtk::ProgressBar,
    drop_revealer: gtk::Revealer,
    minimal_preview: Rc<Cell<bool>>,
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
            .title(APP_NAME)
            .default_width(720)
            .default_height(720)
            .build()
    }

    fn init(
        opened_from_file: bool,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let mut model = Self::new();
        let (width, height) = fit_window_size(
            model.settings.values().window_width,
            model.settings.values().window_height,
        );
        root.set_default_size(width, height);
        util::install_css(model.applied_ui_scale);
        model.opened_from_file = opened_from_file;
        model.preview_window = opened_from_file;

        OPEN_SENDER.with(|slot| *slot.borrow_mut() = Some(sender.clone()));
        let pending =
            PENDING_OPEN.with(|pending| pending.borrow_mut().drain(..).collect::<Vec<_>>());

        let widgets = Self::build_shell(&root, &sender);
        Self::rebuild_list(&mut model, &widgets, &sender);
        Self::show_page(&model, &widgets);

        if !model.minimal_preview() && model.settings.values().is_maximized {
            root.maximize();
        }

        if !pending.is_empty() {
            sender.input(Msg::OpenedFiles(pending));
        } else if !opened_from_file && model.should_show_welcome() {
            sender.input(Msg::Welcome);
        }

        if !opened_from_file
            && system::running_mode() == RunningMode::Extracted
            && !model.should_show_welcome()
        {
            sender.input(Msg::ExtractionNotice);
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
                Self::rebuild_list(self, widgets, &sender);
                Self::rebuild_details(self, widgets, &sender);
            }
            Msg::ShowList => {
                if self.page == Page::MultiUpdate && self.cancel_token.is_none() {
                    self.clear_multi_state();
                }
                self.external_open = false;
                self.preview_window = false;
                self.page = Page::List;
                self.details = None;
                self.reloaded = None;
                Self::show_page(self, widgets);
                Self::rebuild_list(self, widgets, &sender);
            }
            Msg::OpenPath(path) => {
                self.external_open = false;
                self.preview_window = false;
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
                Self::rebuild_list(self, widgets, &sender);
            }
            Msg::OpenChooser => Self::open_chooser(root, &sender),
            Msg::OpenedFiles(paths) => {
                widgets.window.present();
                self.files_chosen(widgets, &sender, paths, LoadSource::External);
            }
            Msg::FilesChosen(paths) => {
                self.files_chosen(widgets, &sender, paths, LoadSource::Chosen);
            }
            Msg::FilesLoaded(loaded, failures) => {
                Self::show_load_failures(widgets, failures);

                let foreign: Vec<String> = if loaded.len() == 1 {
                    loaded
                        .iter()
                        .filter(|file| {
                            self.installed_path(file).is_none()
                                && !file.candidate.arch.is_compatible_with_host()
                        })
                        .map(|file| file.candidate.path.display().to_string())
                        .collect()
                } else {
                    Vec::new()
                };

                if foreign.is_empty() {
                    Self::apply_loaded_files(self, widgets, &sender, loaded);
                } else {
                    self.pending_arch = Some(loaded);
                    Self::confirm_foreign_arch(widgets, &sender, &foreign);
                }
            }
            Msg::ArchDecision(proceed) => {
                let Some(loaded) = self.pending_arch.take() else {
                    return;
                };

                if proceed {
                    Self::apply_loaded_files(self, widgets, &sender, loaded);
                    return;
                }

                self.pending.clear();
                self.multi_ignored.clear();
                self.reset_batch();
                self.details = None;
                self.details_source = None;
                self.reloaded = None;

                if self.minimal_preview() {
                    widgets.window.close();
                } else {
                    self.external_open = false;
                    self.page = Page::List;
                    Self::show_page(self, widgets);
                    Self::rebuild_list(self, widgets, &sender);
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
            Msg::CheckUpdates => Self::start_update_check(self, widgets, &sender, None),
            Msg::CheckAppUpdates(path) => {
                Self::start_update_check(self, widgets, &sender, Some(&path));
            }
            Msg::UpdateCheckStarted(total) => {
                self.check_total = total;
                self.check_done = 0;
                if total > 0 {
                    widgets.updates_progress.set_fraction(0.0);
                }
            }
            Msg::UpdateChecked(path, available, manager) => {
                self.check_done = self.check_done.saturating_add(1);
                if self.check_total > 0 {
                    widgets
                        .updates_progress
                        .set_fraction(progress_fraction(self.check_done, self.check_total));
                }

                if available && !self.config.updates_disabled(&path) {
                    self.updates.insert(path.clone());
                    if let Some(manager) = manager {
                        self.managers.insert(path.clone(), manager);
                    }
                } else {
                    self.updates.remove(&path);
                    self.managers.remove(&path);
                }

                if let Some(DetailsItem::Installed(app)) = &self.details
                    && app.image_path == path
                {
                    Self::rebuild_details(self, widgets, &sender);
                }
            }
            Msg::UpdateRetryStarted => {
                widgets
                    .updates_button
                    .set_label(&t("Retrying failed checks..."));
            }
            Msg::CheckDone => {
                self.checking = false;
                widgets.updates_button.set_sensitive(true);

                if self.check_total > 0 {
                    widgets.updates_progress.set_fraction(1.0);
                    widgets.updates_progress.add_css_class("success");

                    let generation = widgets.updates_generation.get();
                    let progress = widgets.updates_progress.clone();
                    let generation_ref = Rc::clone(&widgets.updates_generation);
                    glib::timeout_add_local_once(Duration::from_millis(400), move || {
                        if generation_ref.get() == generation {
                            progress.set_visible(false);
                        }
                    });
                } else {
                    widgets.updates_progress.set_visible(false);
                }

                widgets.updates_button.set_label(&if self.has_updates() {
                    t("Check for updates")
                } else {
                    t("No updates found")
                });
                Self::rebuild_list(self, widgets, &sender);
            }
            Msg::GithubTokenTested(check) => {
                let store = TokenStore::new(self.settings.clone());
                match check {
                    TokenCheck::Valid { expires_at, .. } => {
                        let _ = store.mark_valid(expires_at.as_deref());
                    }
                    TokenCheck::Invalid => {
                        let _ = store.mark_invalid();
                    }
                    TokenCheck::RateLimited => {
                        let expires_at = store.expires_at();
                        let _ = store.mark_valid(expires_at.as_deref());
                    }
                    _ => {}
                }
            }
            Msg::GithubTokenInvalid => {
                let _ = TokenStore::new(self.settings.clone()).mark_invalid();
                let toast = adw::Toast::new(&t("GitHub token expired or invalid"));
                toast.set_button_label(Some(&t("Open Preferences")));
                toast.set_action_name(Some("win.preferences"));
                widgets.toast.add_toast(toast);
            }
            Msg::GithubRateLimited { info, token_state } => {
                let toast = adw::Toast::new(&rate_limit::message(&info, token_state));
                toast.set_button_label(Some(&t("Open Preferences")));
                toast.set_action_name(Some("win.preferences"));
                widgets.toast.add_toast(toast);
            }
            Msg::InstallCandidate(path) => {
                if self.install_running {
                    Self::toast(widgets, &t("An installation is already running"));
                    return;
                }
                if self.cancel_token.is_some() {
                    Self::toast(widgets, &t("An update is already running"));
                    return;
                }

                let Some(metadata) = self.pending_metadata(&path).cloned() else {
                    Self::toast(widgets, &t("Cannot read the AppImage metadata"));
                    return;
                };

                if conflict_app(&self.apps, &metadata).is_some() {
                    Self::confirm_conflict(
                        widgets,
                        path,
                        &metadata.name,
                        ConflictDialogScope::Single,
                        &sender,
                    );
                    return;
                }

                Self::start_install(self, widgets, path, Conflict::KeepBoth, &sender);
            }
            Msg::InstallWithConflict { path, replace } => {
                if self.install_running {
                    Self::toast(widgets, &t("An installation is already running"));
                    return;
                }

                let conflict = if replace {
                    Conflict::Replace
                } else {
                    Conflict::KeepBoth
                };
                Self::start_install(self, widgets, path, conflict, &sender);
            }
            Msg::InstallAll => {
                if self.install_running {
                    Self::toast(widgets, &t("An installation is already running"));
                    return;
                }
                if self.cancel_token.is_some() {
                    Self::toast(widgets, &t("An update is already running"));
                    return;
                }
                if self.pending.is_empty() {
                    Self::toast(
                        widgets,
                        &t("None of the selected AppImages can be integrated"),
                    );
                    return;
                }

                self.batch_conflicts = self
                    .pending
                    .iter()
                    .filter(|candidate| conflict_app(&self.apps, &candidate.metadata).is_some())
                    .map(|candidate| candidate.path.clone())
                    .collect();
                self.batch_decisions.clear();
                self.batch_skipped.clear();
                self.batch_ask_index = 0;

                if self.batch_conflicts.is_empty() {
                    Self::start_batch_install(self, widgets, &sender);
                } else if self.batch_conflicts.len() == 1 {
                    Self::show_next_batch_conflict(self, widgets, &sender);
                } else {
                    Self::confirm_batch_conflict(widgets, self.batch_conflicts.len(), &sender);
                }
            }
            Msg::BatchPolicy(policy) => match policy {
                BatchConflictPolicy::ReplaceAll => {
                    for path in &self.batch_conflicts {
                        self.batch_decisions.insert(path.clone(), Conflict::Replace);
                    }
                    Self::start_batch_install(self, widgets, &sender);
                }
                BatchConflictPolicy::KeepAll => {
                    Self::start_batch_install(self, widgets, &sender);
                }
                BatchConflictPolicy::AskEach => {
                    self.batch_ask_index = 0;
                    Self::show_next_batch_conflict(self, widgets, &sender);
                }
            },
            Msg::BatchConflictDecision { path, decision } => {
                match decision {
                    BatchConflictDecision::Replace => {
                        self.batch_decisions.insert(path, Conflict::Replace);
                    }
                    BatchConflictDecision::KeepBoth => {
                        self.batch_decisions.insert(path, Conflict::KeepBoth);
                    }
                    BatchConflictDecision::Skip => {
                        self.batch_skipped.insert(path);
                    }
                }

                self.batch_ask_index = self.batch_ask_index.saturating_add(1);
                if self.batch_ask_index < self.batch_conflicts.len() {
                    Self::show_next_batch_conflict(self, widgets, &sender);
                } else {
                    Self::start_batch_install(self, widgets, &sender);
                }
            }
            Msg::BatchConflictCancelled => {
                self.reset_batch();
                if self.page == Page::MultiInstall {
                    Self::rebuild_multi(self, widgets, &sender);
                }
            }
            Msg::Installed(result) => {
                self.install_running = false;
                Self::hide_progress(widgets);
                match result {
                    Ok((app, config)) => {
                        self.config = config;
                        let _ = self.config.save(&self.paths.config_file());
                        self.pending.clear();
                        self.multi_ignored.clear();
                        self.reset_batch();
                        self.reloaded = None;
                        self.refresh_apps();
                        let installed = self
                            .apps
                            .iter()
                            .find(|candidate| candidate.image_path == app.image_path)
                            .cloned();

                        if installed.as_ref().is_some_and(|app| app.self_entry) {
                            self.refresh_autostart();
                        }

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
                        if self.minimal_preview() {
                            Self::minimal_ui(widgets);
                        }
                        Self::toast(widgets, &t("AppImage integrated"));
                    }
                    Err(error) => Self::toast(widgets, &error),
                }
            }
            Msg::MultiInstalled(result) => {
                self.install_running = false;
                Self::hide_progress(widgets);
                match result {
                    Ok(result) => {
                        self.config = result.config;
                        let _ = self.config.save(&self.paths.config_file());

                        let integrated_self = self.pending.iter().any(|candidate| {
                            self_install::is_self_metadata(&candidate.metadata)
                                && result.integrated.contains(&candidate.path)
                        });

                        // Only failed and skipped AppImages stay on the batch
                        // page, so the user can retry them without adding the
                        // files again.
                        retain_pending(&mut self.pending, &result.failed, &self.batch_skipped);

                        let integrated = result.integrated.len();
                        let failed = result.failed.len();
                        let remaining = self.pending.len();
                        self.multi_ignored.clear();
                        self.reset_batch();
                        self.reloaded = None;
                        self.refresh_apps();
                        if integrated_self {
                            self.refresh_autostart();
                        }

                        if remaining == 0 {
                            self.preview_window = false;
                            self.page = Page::List;
                            Self::show_page(self, widgets);
                            Self::rebuild_list(self, widgets, &sender);
                            Self::toast(widgets, &t("AppImages integrated"));
                        } else {
                            self.page = Page::MultiInstall;
                            Self::show_page(self, widgets);
                            Self::rebuild_multi(self, widgets, &sender);
                            if integrated > 0 {
                                Self::toast(
                                    widgets,
                                    &tf(
                                        "{done} integrated; {remaining} pending",
                                        &[
                                            ("done", &integrated.to_string()),
                                            ("remaining", &remaining.to_string()),
                                        ],
                                    ),
                                );
                            } else if failed > 0 {
                                Self::toast(widgets, &t("No AppImage could be integrated"));
                            } else {
                                Self::toast(widgets, &t("Some AppImages were skipped"));
                            }
                        }
                    }
                    Err(error) => {
                        self.multi_ignored.clear();
                        self.reset_batch();
                        if self.pending.is_empty() {
                            self.preview_window = false;
                            self.page = Page::List;
                            Self::show_page(self, widgets);
                            Self::rebuild_list(self, widgets, &sender);
                        } else {
                            self.page = Page::MultiInstall;
                            Self::show_page(self, widgets);
                            Self::rebuild_multi(self, widgets, &sender);
                        }
                        Self::toast(widgets, &error);
                    }
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
                    self.updates.remove(&path);
                    self.managers.remove(&path);

                    if self.removing_self {
                        self.removing_self = false;
                        widgets.window.close();
                        return;
                    }

                    if self.preview_window {
                        widgets.window.close();
                        return;
                    }

                    self.page = Page::List;
                    self.details = None;
                    self.refresh_apps();
                    Self::show_page(self, widgets);
                    Self::rebuild_list(self, widgets, &sender);
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

                self.updates.clear();
                self.managers.clear();
                self.refresh_apps();
                Self::rebuild_list(self, widgets, &sender);
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
                        Self::launch(app, &sender, self.window_after_launch());
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
            Msg::CloseAfterLaunch => widgets.window.close(),
            Msg::StartUpdate(path) => {
                if self.cancel_token.is_some() {
                    Self::toast(widgets, &t("An update is already running"));
                    return;
                }
                if self.install_running {
                    Self::toast(widgets, &t("An installation is already running"));
                    return;
                }

                let Some(app) = self.apps.iter().find(|app| app.image_path == path).cloned() else {
                    return;
                };

                if is_running(&app.image_path) {
                    Self::toast(widgets, &t("The application is running, close it first"));
                    return;
                }

                let options = self.update_options();
                let config = self.config.clone();
                let paths = self.paths.clone();
                let stored_manager = self.managers.get(&path).cloned();
                let download_dir = options.staging_dir();
                let cancel = Arc::new(AtomicBool::new(false));
                self.cancel_token = Some(Arc::clone(&cancel));
                self.single_update = Some(SingleUpdate {
                    path: path.clone(),
                    fraction: 0.0,
                    stage: SingleUpdateStage::Running,
                });
                if self.page == Page::Details {
                    Self::rebuild_details(self, widgets, &sender);
                } else {
                    Self::rebuild_list(self, widgets, &sender);
                }

                std::thread::spawn(move || {
                    let mut config = config;
                    let manager = if let Some(manager) = stored_manager {
                        manager
                    } else {
                        let settings = Settings::load(&paths);
                        let token = TokenStore::new(settings).get();
                        match update::resolve(
                            &app.image_path,
                            &config.update_manager(&app.image_path),
                            token.as_deref(),
                        ) {
                            Ok(Some(manager)) => Arc::from(manager),
                            Ok(None) => {
                                sender.input(Msg::Updated(Err(t(
                                    "No update method was found for this AppImage",
                                ))));
                                return;
                            }
                            Err(error) => {
                                sender.input(Msg::Updated(Err(error.to_string())));
                                return;
                            }
                        }
                    };

                    let mut progress = |fraction: f64| {
                        sender.input(Msg::SingleProgress(fraction));
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
                if self.install_running {
                    Self::toast(widgets, &t("An installation is already running"));
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

                self.clear_multi_state();
                self.update_queue.clone_from(&queue);
                self.page = Page::MultiUpdate;
                Self::show_page(self, widgets);
                Self::rebuild_multi(self, widgets, &sender);

                let config = self.config.clone();
                let paths = self.paths.clone();
                let options = self.update_options();
                let stored_managers = self.managers.clone();
                let download_dir = options.staging_dir();
                let parallel = self.settings.values().max_parallel_downloads;
                let cancel = Arc::new(AtomicBool::new(false));
                self.cancel_token = Some(Arc::clone(&cancel));
                widgets.multi_progress.set_visible(true);
                widgets.multi_progress.set_fraction(0.0);

                std::thread::spawn(move || {
                    let mut config = config;
                    let mut jobs = Vec::with_capacity(queue.len());
                    let mut token: Option<String> = None;
                    let mut token_loaded = false;

                    for app in queue {
                        let resolved: Result<Option<Arc<dyn update::UpdateManager>>, CoreError> =
                            if let Some(manager) = stored_managers.get(&app.image_path) {
                                Ok(Some(Arc::clone(manager)))
                            } else {
                                if !token_loaded {
                                    let settings = Settings::load(&paths);
                                    token = TokenStore::new(settings).get();
                                    token_loaded = true;
                                }

                                update::resolve(
                                    &app.image_path,
                                    &config.update_manager(&app.image_path),
                                    token.as_deref(),
                                )
                                .map(|manager| manager.map(Arc::from))
                            };

                        match resolved {
                            Ok(Some(manager)) => {
                                jobs.push(update::UpdateJob::new(app, manager));
                            }
                            Ok(None) => {
                                sender.input(Msg::MultiAppFailed {
                                    path: app.image_path,
                                    error: t("No update method was found for this AppImage"),
                                });
                            }
                            Err(error) => {
                                tracing::warn!(
                                    app = %app.name,
                                    %error,
                                    "cannot resolve the update manager"
                                );
                                sender.input(Msg::MultiAppFailed {
                                    path: app.image_path,
                                    error: error.to_string(),
                                });
                            }
                        }
                    }

                    if jobs.is_empty() {
                        // Every queued app failed to resolve; keep the page so
                        // each row can show its reason.
                        sender.input(Msg::MultiUpdated(Ok(config), false));
                        return;
                    }

                    let jobs_ref = &jobs;
                    let mut on_event = |event| match event {
                        update::BatchEvent::Started { index } => {
                            if let Some(job) = jobs_ref.get(index) {
                                sender.input(Msg::MultiAppStarted(job.app.image_path.clone()));
                            }
                        }
                        update::BatchEvent::Progress {
                            index,
                            fraction,
                            overall,
                        } => {
                            if let Some(job) = jobs_ref.get(index) {
                                sender.input(Msg::MultiProgress {
                                    path: job.app.image_path.clone(),
                                    fraction,
                                    overall,
                                });
                            }
                        }
                        update::BatchEvent::Installed { app, .. } => {
                            sender.input(Msg::MultiAppUpdated(app.image_path));
                        }
                        update::BatchEvent::Failed { app, error, .. } => {
                            sender.input(Msg::MultiAppFailed {
                                path: app.image_path,
                                error: error.to_string(),
                            });
                        }
                        update::BatchEvent::Cancelled { index } => {
                            if let Some(job) = jobs_ref.get(index) {
                                sender.input(Msg::MultiAppCancelled(job.app.image_path.clone()));
                            }
                        }
                        _ => {}
                    };

                    let report = update::apply_batch(
                        &jobs,
                        &options,
                        &mut config,
                        &download_dir,
                        parallel,
                        &cancel,
                        &mut on_event,
                    );

                    sender.input(Msg::MultiUpdated(Ok(config), report.cancelled));
                });
            }
            Msg::Progress(fraction, text) => {
                if self.page == Page::MultiUpdate {
                    widgets.multi_progress.set_visible(true);
                    widgets.multi_progress.set_fraction(fraction);
                } else {
                    widgets.progress.set_visible(true);
                    widgets.progress.set_fraction(fraction);
                    widgets.progress.set_text(Some(&text));
                }
            }
            Msg::SingleProgress(fraction) => {
                let fraction = fraction.clamp(0.0, 1.0);
                if let Some(update) = &mut self.single_update {
                    update.fraction = fraction;
                }
                if let Some(row) = &self.single_row {
                    row.ring.set_fraction(fraction);
                }
            }
            Msg::MultiProgress {
                path,
                fraction,
                overall,
            } => {
                self.multi_fractions.insert(path.clone(), fraction);
                if let Some(row) = self.multi_rows.get(&path) {
                    row.ring.set_fraction(fraction);
                }
                if self.page == Page::MultiUpdate {
                    widgets.multi_progress.set_visible(true);
                    widgets.multi_progress.set_fraction(overall);
                }
            }
            Msg::CancelUpdate => {
                if let Some(cancel) = &self.cancel_token {
                    cancel.store(true, Ordering::Relaxed);
                    if self.page == Page::MultiUpdate {
                        Self::rebuild_multi(self, widgets, &sender);
                    }
                }
            }
            Msg::UpdateCancelled => {
                self.cancel_token = None;
                self.single_update = None;
                self.single_row = None;
                if self.page == Page::Details {
                    Self::rebuild_details(self, widgets, &sender);
                } else {
                    Self::rebuild_list(self, widgets, &sender);
                }
                Self::toast(widgets, &t("Update cancelled"));
            }
            Msg::Updated(result) => {
                self.cancel_token = None;
                match result {
                    Ok((app, config)) => {
                        self.config = config;
                        let _ = self.config.save(&self.paths.config_file());
                        self.updates.remove(&app.image_path);
                        self.managers.remove(&app.image_path);
                        self.reloaded = None;
                        self.refresh_apps();
                        widgets.update_all_button.set_visible(self.has_updates());

                        if let Some(update) = self.single_update.as_mut()
                            && update.path == app.image_path
                        {
                            update.fraction = 1.0;
                            update.stage = SingleUpdateStage::Done;
                            if let Some(row) = &self.single_row {
                                row.ring.set_fraction(1.0);
                                row.status.set_visible_child_name("done");
                                row.status.set_tooltip_text(None);
                            }
                        }

                        let sender_finished = sender.clone();
                        let path = app.image_path.clone();
                        glib::timeout_add_local_once(SINGLE_UPDATE_CHECK_DURATION, move || {
                            sender_finished.input(Msg::SingleUpdateFinished(path));
                        });
                        Self::toast(widgets, &tf("{name} updated", &[("name", &app.name)]));
                    }
                    Err(error) => {
                        self.single_update = None;
                        self.single_row = None;
                        if self.page == Page::Details {
                            Self::rebuild_details(self, widgets, &sender);
                        } else {
                            Self::rebuild_list(self, widgets, &sender);
                        }
                        Self::toast(widgets, &error);
                    }
                }
            }
            Msg::SingleUpdateFinished(path) => {
                if self
                    .single_update
                    .as_ref()
                    .is_none_or(|update| update.path != path)
                {
                    return;
                }

                self.single_update = None;
                self.single_row = None;
                self.updates.remove(&path);
                self.managers.remove(&path);

                if self.page == Page::Details {
                    if let Some(DetailsItem::Installed(app)) = &self.details
                        && app.image_path == path
                    {
                        self.details = self
                            .apps
                            .iter()
                            .find(|candidate| candidate.image_path == path)
                            .cloned()
                            .map(DetailsItem::Installed);
                    }
                    Self::rebuild_details(self, widgets, &sender);
                }
                Self::rebuild_list(self, widgets, &sender);
            }
            Msg::MultiAppStarted(path) => {
                self.multi_active.insert(path.clone());
                if let Some(row) = self.multi_rows.get(&path) {
                    row.row.set_opacity(1.0);
                    row.ring.activate();
                    if let Some(app) = self.update_queue.iter().find(|app| app.image_path == path) {
                        row.status.set_tooltip_text(Some(&tf(
                            "Updating {name}...",
                            &[("name", &app.name)],
                        )));
                    }
                }
            }
            Msg::MultiAppUpdated(path) => {
                self.multi_done.insert(path.clone());
                self.multi_active.remove(&path);
                self.multi_fractions.insert(path.clone(), 1.0);
                self.updates.remove(&path);
                self.managers.remove(&path);
                if let Some(row) = self.multi_rows.get(&path) {
                    row.row.set_opacity(1.0);
                    row.ring.set_fraction(1.0);
                    row.status.set_visible_child_name("done");
                    row.status.set_tooltip_text(None);
                    row.error.set_visible(false);
                }
            }
            Msg::MultiAppCancelled(path) => {
                self.multi_active.remove(&path);
                self.multi_cancelled.insert(path.clone());
                if let Some(row) = self.multi_rows.get(&path) {
                    row.row.set_opacity(1.0);
                    row.status.set_visible_child_name("cancelled");
                    row.status.set_tooltip_text(Some(&t("Update cancelled")));
                }
            }
            Msg::MultiAppFailed { path, error } => {
                self.multi_active.remove(&path);
                self.multi_fractions.insert(path.clone(), 1.0);
                self.multi_failed.insert(path.clone(), error.clone());
                if let Some(row) = self.multi_rows.get(&path) {
                    row.row.set_opacity(1.0);
                    row.row.set_tooltip_text(Some(&error));
                    row.status.set_visible_child_name("failed");
                    row.status.set_tooltip_text(Some(&error));
                    row.error.set_label(&error);
                    row.error.set_tooltip_text(Some(&error));
                    row.error.set_visible(true);
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
                        // Remove only the badges that were actually installed;
                        // failed updates must stay visible in the list.
                        for path in &self.multi_done {
                            self.updates.remove(path);
                            self.managers.remove(path);
                        }
                        self.refresh_apps();
                        if self.multi_failed.is_empty() && !cancelled {
                            self.clear_multi_state();
                            self.page = Page::List;
                            Self::show_page(self, widgets);
                            Self::rebuild_list(self, widgets, &sender);
                            Self::toast(widgets, &t("Updates installed"));
                        } else {
                            if cancelled {
                                for app in &self.update_queue {
                                    if !self.multi_done.contains(&app.image_path)
                                        && !self.multi_failed.contains_key(&app.image_path)
                                    {
                                        self.multi_active.remove(&app.image_path);
                                        self.multi_cancelled.insert(app.image_path.clone());
                                    }
                                }
                                Self::toast(widgets, &t("Updates cancelled"));
                            }
                            self.multi_finished = true;
                            Self::show_page(self, widgets);
                            Self::rebuild_multi(self, widgets, &sender);
                        }
                    }
                    Err(error) => {
                        self.clear_multi_state();
                        self.page = Page::List;
                        Self::show_page(self, widgets);
                        Self::rebuild_list(self, widgets, &sender);
                        Self::toast(widgets, &error);
                    }
                }
            }
            Msg::SaveExec {
                path,
                env,
                disabled_env,
                arguments,
            } => {
                if let Some(app) = self.apps.iter().find(|app| app.image_path == path).cloned() {
                    match install::set_exec(&app, &env, &arguments) {
                        Ok(()) => {
                            let mut config = self.config.clone();
                            config.set_disabled_env(&path, &disabled_env);
                            let history: Vec<String> = env
                                .iter()
                                .chain(disabled_env.iter())
                                .filter(|entry| is_history_variable(entry.as_str()))
                                .cloned()
                                .collect();
                            config.remember_environment(&history);
                            if let Err(error) = config.save(&self.paths.config_file()) {
                                Self::toast(widgets, &error.to_string());
                            }
                            // `Exec` already changed, so keep the in-memory
                            // configuration in sync even when the file could
                            // not be written; the toast reports the failure.
                            self.config = config;

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
            Msg::ForgetEnvironment(entry) => {
                let mut config = self.config.clone();
                if config.forget_environment(&entry) {
                    if let Err(error) = config.save(&self.paths.config_file()) {
                        Self::toast(widgets, &error.to_string());
                    }
                    self.config = config;
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
            Msg::SetUpdatesEnabled { path, enabled } => {
                let mut config = self.config.clone();
                if enabled {
                    config.enable_updates(&path);
                } else {
                    config.disable_updates(&path);
                }

                match config.save(&self.paths.config_file()) {
                    Ok(()) => {
                        self.config = config;

                        if !enabled && self.updates.remove(&path) {
                            self.managers.remove(&path);
                            Self::rebuild_details(self, widgets, &sender);
                            Self::rebuild_list(self, widgets, &sender);
                        }

                        if enabled {
                            sender.input(Msg::CheckAppUpdates(path));
                        }
                    }
                    Err(error) => {
                        Self::toast(widgets, &error.to_string());
                        Self::rebuild_details(self, widgets, &sender);
                    }
                }
            }
            Msg::SetParallelDownloads(value) => self.settings.set_max_parallel_downloads(value),
            Msg::SetParallelChecks(value) => self.settings.set_max_parallel_checks(value),
            Msg::UiScaleChanged(percent) => self.settings.set_ui_scale(percent),
            Msg::RestartApp => Self::restart_self(self, widgets),
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
                    Self::rebuild_list(self, widgets, &sender);
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
                    self.managers.remove(&path);
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
                self.managers.remove(&path);
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
            Msg::ExtractionNotice => {
                let toast = adw::Toast::new(&t("Running in extraction mode"));
                toast.set_button_label(Some(&t("Check FUSE")));
                toast.set_action_name(Some("win.preferences"));
                widgets.toast.add_toast(toast);
            }
            Msg::Welcome => {
                widgets
                    .window
                    .set_size_request(welcome::WELCOME_MAIN_MIN_WIDTH, -1);
                widgets.window.present();
                widgets.welcome = Some(welcome::present(&widgets.window, &self.settings, &sender));
            }
            Msg::WelcomeRecheckDependencies => {
                if let Some(welcome) = &widgets.welcome {
                    welcome.recheck_dependencies(&sender);
                }
            }
            Msg::WelcomeFolder(path) => {
                self.settings.set_appimages_dir(&path);
                if let Some(welcome) = &widgets.welcome {
                    welcome.folder_row.set_subtitle(&path.to_string_lossy());
                }
                self.refresh_apps();
                Self::rebuild_list(self, widgets, &sender);
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
                    widgets.window.set_size_request(-1, -1);
                    tracing::info!(
                        image = %installation.image_path.display(),
                        desktop = %installation.desktop_path.display(),
                        "Gear LeverR installed"
                    );
                    self.refresh_apps();
                    self.refresh_autostart();
                    Self::rebuild_list(self, widgets, &sender);
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
                widgets.window.set_size_request(-1, -1);
            }
            Msg::WelcomeClosed => {
                widgets.window.set_size_request(-1, -1);
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
        let applied_ui_scale = settings.values().ui_scale;

        let mut model = Self {
            paths,
            settings,
            applied_ui_scale,
            config,
            apps: Vec::new(),
            page: Page::List,
            details: None,
            details_source: None,
            search: String::new(),
            updates: HashSet::new(),
            managers: HashMap::new(),
            checking: false,
            check_total: 0,
            check_done: 0,
            pending: Vec::new(),
            pending_arch: None,
            multi_ignored: Vec::new(),
            batch_conflicts: Vec::new(),
            batch_decisions: HashMap::new(),
            batch_skipped: HashSet::new(),
            batch_ask_index: 0,
            install_running: false,
            multi_done: HashSet::new(),
            multi_active: HashSet::new(),
            multi_fractions: HashMap::new(),
            multi_cancelled: HashSet::new(),
            multi_failed: HashMap::new(),
            multi_rows: HashMap::new(),
            multi_finished: false,
            single_update: None,
            single_row: None,
            update_queue: Vec::new(),
            opened_from_file: false,
            preview_window: false,
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

    /// Returns whether the window was created to preview an externally opened file.
    fn minimal_preview(&self) -> bool {
        self.preview_window && self.settings.values().preview_minimal_ui
    }

    /// Returns whether launching an installed app should close the window.
    ///
    /// Gear LeverR only closes when the window was created to handle an AppImage;
    /// launching from an already running window keeps it open.
    fn window_after_launch(&self) -> WindowAfterLaunch {
        if self.preview_window {
            WindowAfterLaunch::Close
        } else {
            WindowAfterLaunch::StayOpen
        }
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

        self.remember_current_environment();
    }

    /// Rewrites the login update command after the self AppImage changed name.
    fn refresh_autostart(&self) {
        if !self.settings.values().fetch_in_background || !self_install::is_installed(&self.paths) {
            return;
        }

        if let Err(error) = crate::autostart::apply(&self.paths, true) {
            tracing::warn!(%error, "cannot refresh the Gear LeverR autostart entry");
        }
    }

    /// Keeps the global history in sync with the variables currently in use.
    ///
    /// Seeding from the installed apps means a variable already present in a
    /// desktop entry stays in the history when it is later deleted, instead of
    /// disappearing with the only app that used it.
    fn remember_current_environment(&mut self) {
        let mut entries = Vec::new();

        for app in &self.apps {
            entries.extend(
                app.env_variables
                    .iter()
                    .filter(|entry| is_history_variable(entry.as_str()))
                    .cloned(),
            );
            entries.extend(
                self.config
                    .disabled_env(&app.image_path)
                    .into_iter()
                    .filter(|entry| is_history_variable(entry)),
            );
        }

        let pruned = self.config.prune_environment_removed(&entries);
        let remembered = self.config.remember_missing_environment(&entries);
        if (pruned || remembered)
            && let Err(error) = self.config.save(&self.paths.config_file())
        {
            tracing::warn!(%error, "cannot persist the environment variable history");
        }
    }

    /// Returns the environment variables used by any installed application.
    ///
    /// The persisted history comes first, followed by the variables currently
    /// configured for each app, so integrations created before the history
    /// existed show their variables without a migration.
    fn environment_history(&self) -> Vec<String> {
        let mut entries = self.config.environment_history();

        for app in &self.apps {
            entries.extend(
                app.env_variables
                    .iter()
                    .filter(|entry| is_history_variable(entry.as_str()))
                    .cloned(),
            );
            entries.extend(
                self.config
                    .disabled_env(&app.image_path)
                    .into_iter()
                    .filter(|entry| is_history_variable(entry)),
            );
        }

        let mut seen = HashSet::new();
        let removed = self.config.environment_removed();
        entries.retain(|entry| {
            !removed.contains(entry) && is_history_variable(entry) && seen.insert(entry.clone())
        });
        entries
    }

    fn start_update_check(
        &mut self,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
        only: Option<&Path>,
    ) {
        if self.checking {
            return;
        }

        self.checking = true;
        self.check_total = 0;
        self.check_done = 0;
        widgets.updates_button.set_sensitive(false);
        widgets.updates_button.set_label(&t("Checking updates..."));
        widgets.updates_progress.set_visible(true);
        widgets.updates_progress.set_fraction(0.0);
        widgets.updates_progress.remove_css_class("success");
        widgets
            .updates_generation
            .set(widgets.updates_generation.get().wrapping_add(1));

        let apps: Vec<InstalledApp> = match only {
            Some(path) => self
                .apps
                .iter()
                .filter(|app| app.image_path == path)
                .cloned()
                .collect(),
            None => self.apps.clone(),
        };
        let config = self.config.clone();
        let paths = self.paths.clone();
        let hash_dir = self.paths.cache_root().join("hashes");
        let check_workers = self.settings.values().max_parallel_checks;
        let sender = sender.clone();

        std::thread::spawn(move || {
            let token_store = TokenStore::new(Settings::load(&paths));
            let token = token_store.get();
            let token_state = if token.is_some() {
                TokenState::Used
            } else if token_store.status() != TokenStatus::None {
                TokenState::ConfiguredNotLoaded
            } else {
                TokenState::None
            };
            let mut jobs = Vec::new();

            for app in apps {
                if config.updates_disabled(&app.image_path) {
                    continue;
                }

                match update::resolve(
                    &app.image_path,
                    &config.update_manager(&app.image_path),
                    token.as_deref(),
                ) {
                    Ok(Some(manager)) => {
                        jobs.push(update::UpdateJob::new(app, Arc::from(manager)));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            app = %app.name,
                            %error,
                            "cannot resolve the update manager"
                        );
                    }
                }
            }

            sender.input(Msg::UpdateCheckStarted(jobs.len()));

            if !jobs.is_empty() {
                let hashes = HashCache::new(hash_dir);
                let jobs_ref = &jobs;
                // One notice per check pass; the next user-initiated check
                // resets it and shows the limit again.
                let rate_limit_notified = Cell::new(false);
                let mut on_event = |event| match event {
                    update::CheckEvent::Checked { index, status } => {
                        let job = &jobs_ref[index];
                        let manager =
                            (status == UpdateStatus::Available).then(|| Arc::clone(&job.manager));
                        sender.input(Msg::UpdateChecked(
                            job.app.image_path.clone(),
                            status == UpdateStatus::Available,
                            manager,
                        ));
                    }
                    update::CheckEvent::Retrying => sender.input(Msg::UpdateRetryStarted),
                    update::CheckEvent::TokenInvalid => sender.input(Msg::GithubTokenInvalid),
                    update::CheckEvent::RateLimited(info) => {
                        rate_limit_notified.set(true);
                        sender.input(Msg::GithubRateLimited { info, token_state });
                    }
                    _ => {}
                };

                let report = update::check_batch(
                    jobs_ref,
                    &hashes,
                    check_workers,
                    RETRY_BUDGET,
                    &mut on_event,
                );

                if !rate_limit_notified.get()
                    && let Some(info) = report.rate_limit
                {
                    sender.input(Msg::GithubRateLimited { info, token_state });
                }
            }

            sender.input(Msg::CheckDone);
        });
    }

    fn updates_toggle_row(
        &self,
        app: &InstalledApp,
        sender: &ComponentSender<Self>,
    ) -> adw::ActionRow {
        let enabled = !self.config.updates_disabled(&app.image_path);
        let row = adw::ActionRow::builder()
            .title(t("Check for updates"))
            .subtitle(t("Look for new versions of this application"))
            .build();
        row.set_activatable(true);
        row.add_prefix(&gtk::Image::from_icon_name(
            "software-update-available-symbolic",
        ));

        let toggle = gtk::Switch::new();
        toggle.set_valign(gtk::Align::Center);
        toggle.set_active(enabled);
        row.add_suffix(&toggle);

        // AdwSwitchRow (libadwaita 1.4) is not available on Debian 12.
        let toggle_for_row = toggle.clone();
        row.connect_activated(move |_| {
            toggle_for_row.set_active(!toggle_for_row.is_active());
        });

        let sender = sender.clone();
        let path = app.image_path.clone();
        toggle.connect_active_notify(move |toggle| {
            let enabled = toggle.is_active();
            let sender = sender.clone();
            let path = path.clone();
            glib::idle_add_local_once(move || {
                sender.input(Msg::SetUpdatesEnabled { path, enabled });
            });
        });

        row
    }

    fn pending_metadata(&self, path: &Path) -> Option<&AppMetadata> {
        if let Some(DetailsItem::Candidate(candidate)) = &self.details
            && candidate.path == path
        {
            return Some(&candidate.metadata);
        }

        self.pending
            .iter()
            .find(|candidate| candidate.path == path)
            .map(|candidate| &candidate.metadata)
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
        &mut self,
        widgets: &Widgets,
        path: PathBuf,
        conflict: Conflict,
        sender: &ComponentSender<Self>,
    ) {
        if self.install_running {
            Self::toast(widgets, &t("An installation is already running"));
            return;
        }
        if self.cancel_token.is_some() {
            Self::toast(widgets, &t("An update is already running"));
            return;
        }

        let Some(metadata) = self.pending_metadata(&path).cloned() else {
            Self::toast(widgets, &t("Cannot read the AppImage metadata"));
            return;
        };

        let previous = if conflict == Conflict::Replace {
            conflict_app(&self.apps, &metadata).map(previous_install)
        } else {
            None
        };

        let options = self.integrate_options(conflict, None);
        let config = self.config.clone();
        Self::show_progress(widgets);
        widgets.progress.set_fraction(0.0);
        widgets.progress.set_text(Some(&t("Integrating...")));
        self.install_running = true;

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

    /// Starts the integrations that were not skipped.
    fn start_batch_install(&mut self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        if self.install_running {
            Self::toast(widgets, &t("An installation is already running"));
            return;
        }
        if self.cancel_token.is_some() {
            Self::toast(widgets, &t("An update is already running"));
            return;
        }

        let jobs = install_jobs(
            &self.pending,
            &self.apps,
            &self.batch_decisions,
            &self.batch_skipped,
        );

        if jobs.is_empty() {
            self.reset_batch();
            Self::rebuild_multi(self, widgets, sender);
            Self::toast(widgets, &t("Some AppImages were skipped"));
            return;
        }

        let options = self.integrate_options(Conflict::KeepBoth, None);
        let config = self.config.clone();
        let total = jobs.len();
        Self::show_progress(widgets);
        widgets.progress.set_fraction(0.0);
        widgets.progress.set_text(Some(&t("Integrating...")));
        self.install_running = true;
        Self::rebuild_multi(self, widgets, sender);

        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut config = config;
            let mut integrated = Vec::new();
            let mut failed = Vec::new();
            let mut done = 0_usize;

            for (index, job) in jobs.iter().enumerate() {
                let mut job_options = options.clone();
                job_options.conflict = job.conflict;

                match install::integrate(
                    &job.path,
                    &job.metadata,
                    job.previous.as_ref(),
                    &job_options,
                    &mut config,
                ) {
                    Ok(_) => {
                        done = done.saturating_add(1);
                        integrated.push(job.path.clone());
                    }
                    Err(error) => {
                        failed.push(job.path.clone());
                        sender.input(Msg::Toast(format!("{}: {error}", job.path.display())));
                    }
                }

                sender.input(Msg::Progress(
                    fraction(index + 1, total),
                    tf(
                        "Integrated {done} of {total}",
                        &[("done", &done.to_string()), ("total", &total.to_string())],
                    ),
                ));
            }

            sender.input(Msg::MultiInstalled(Ok(BatchInstallResult {
                config,
                integrated,
                failed,
            })));
        });
    }

    /// Shows the conflict dialog for the next AppImage of the batch.
    fn show_next_batch_conflict(&mut self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        while let Some(path) = self.batch_conflicts.get(self.batch_ask_index).cloned() {
            if let Some(metadata) = self.pending_metadata(&path).cloned() {
                Self::confirm_conflict(
                    widgets,
                    path,
                    &metadata.name,
                    ConflictDialogScope::Batch,
                    sender,
                );
                return;
            }
            self.batch_ask_index = self.batch_ask_index.saturating_add(1);
        }

        Self::start_batch_install(self, widgets, sender);
    }

    fn reset_batch(&mut self) {
        self.batch_conflicts.clear();
        self.batch_decisions.clear();
        self.batch_skipped.clear();
        self.batch_ask_index = 0;
    }

    /// Clears every widget and value that belongs to the multi-update page.
    fn clear_multi_state(&mut self) {
        self.update_queue.clear();
        self.multi_done.clear();
        self.multi_active.clear();
        self.multi_fractions.clear();
        self.multi_cancelled.clear();
        self.multi_failed.clear();
        self.multi_rows.clear();
        self.multi_finished = false;
    }

    fn update_options(&self) -> UpdateOptions {
        UpdateOptions {
            appimages_dir: self.settings.values().appimages_dir.clone(),
            applications_dir: self.paths.applications_dir(),
            exec_name_for_terminal_apps: self.settings.values().exec_name_terminal,
            update_desktop_database: true,
            allow_unsafe_extractor: !self.settings.values().block_unsafe_extractor,
            use_zsync_delta: self.settings.values().use_zsync_delta,
        }
    }

    fn files_chosen(
        &mut self,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
        paths: Vec<PathBuf>,
        source: LoadSource,
    ) {
        self.external_open = source == LoadSource::External;
        if source == LoadSource::Chosen {
            self.preview_window = false;
        }
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

    /// Returns the installed path that already matches `file`, if any.
    fn installed_path(&self, file: &LoadedFile) -> Option<PathBuf> {
        file.installed.clone().or_else(|| {
            self.apps
                .iter()
                .find(|app| same_file(&app.image_path, &file.candidate.path))
                .map(|app| app.image_path.clone())
        })
    }

    fn apply_loaded_files(
        &mut self,
        widgets: &Widgets,
        sender: &ComponentSender<Self>,
        loaded: Vec<LoadedFile>,
    ) {
        let multiple = loaded.len() > 1;
        let mut fresh = Vec::new();
        let mut ignored = Vec::new();
        let mut installed_details = None;

        for file in loaded {
            let existing = self.installed_path(&file);

            if multiple {
                if let Some(reason) = ignored_reason(&file, existing) {
                    ignored.push(IgnoredCandidate {
                        candidate: file.candidate,
                        reason,
                    });
                } else {
                    fresh.push(file.candidate);
                }
            } else if let Some(path) = existing {
                installed_details = self.apps.iter().find(|app| app.image_path == path).cloned();
            } else {
                fresh.push(file.candidate);
            }
        }

        self.pending.clear();
        self.multi_ignored.clear();
        self.reset_batch();

        if multiple {
            self.pending = fresh;
            self.multi_ignored = ignored;
            self.details = None;
            self.details_source = None;
            self.reloaded = None;
            self.page = Page::MultiInstall;
            Self::show_page(self, widgets);
            Self::rebuild_multi(self, widgets, sender);
            return;
        }

        if let Some(app) = installed_details {
            self.details = Some(DetailsItem::Installed(app));
            self.details_source = None;
            self.reloaded = None;
            self.page = Page::Details;
            Self::show_page(self, widgets);
            Self::rebuild_details(self, widgets, sender);
            if self.minimal_preview() {
                Self::minimal_ui(widgets);
            }
            return;
        }

        if let Some(candidate) = fresh.into_iter().next() {
            self.details = Some(DetailsItem::Candidate(Box::new(candidate)));
            self.details_source = None;
            self.reloaded = None;
            self.page = Page::Details;
            Self::show_page(self, widgets);
            Self::rebuild_details(self, widgets, sender);
            if self.minimal_preview() {
                Self::minimal_ui(widgets);
            }
        }
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

                match AppImage::open(&path) {
                    Ok(appimage) => {
                        let arch = appimage.arch();
                        match appimage.metadata_with(&options) {
                            Ok(metadata) => {
                                let duplicate = installed
                                    .iter()
                                    .find(|candidate| {
                                        install::is_installed_copy(&path, candidate, &appimages_dir)
                                    })
                                    .cloned();
                                loaded.push(LoadedFile {
                                    candidate: CandidateFile {
                                        path,
                                        metadata,
                                        arch,
                                    },
                                    installed: duplicate,
                                });
                            }
                            Err(error) => {
                                let suffix = if arch.is_compatible_with_host() {
                                    String::new()
                                } else {
                                    format!(" ({arch})")
                                };
                                tracing::warn!(
                                    path = %path.display(),
                                    %error,
                                    "cannot read the AppImage metadata"
                                );
                                let message =
                                    format!("{}{suffix}", load_error_message(&path, &error));
                                errors.push(LoadFailure {
                                    path,
                                    message,
                                    kind: load_failure_kind(&error),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "cannot open the AppImage"
                        );
                        let message = load_error_message(&path, &error);
                        errors.push(LoadFailure {
                            path,
                            message,
                            kind: LoadFailureKind::Other,
                        });
                    }
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

    fn confirm_foreign_arch(widgets: &Widgets, sender: &ComponentSender<Self>, paths: &[String]) {
        let body = paths.join("\n");
        let dialog = adw::MessageDialog::new(
            Some(&widgets.window),
            Some(&t(
                "This app might not be compatible with your system architecture",
            )),
            Some(&body),
        );
        dialog.add_response("cancel", &t("Cancel"));
        dialog.add_response("continue", &t("Continue"));
        dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let sender = sender.clone();
        dialog.connect_response(None, move |dialog: &adw::MessageDialog, response: &str| {
            dialog.close();
            sender.input(Msg::ArchDecision(response == "continue"));
        });

        dialog.present();
    }

    fn show_page(&self, widgets: &Widgets) {
        let (name, title, icon) = match self.page {
            Page::List => ("list", APP_NAME.to_owned(), "list-add-symbolic"),
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
        let show_left_button = match self.page {
            Page::MultiUpdate => self.multi_finished,
            _ => self.page.shows_left_button(self.preview_window),
        };
        widgets.left_button.set_visible(show_left_button);
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

    fn rebuild_list(&mut self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        clear_listbox(&widgets.updates_list);
        clear_listbox(&widgets.installed_list);
        if self.page == Page::List {
            self.single_row = None;
        }

        let query = normalize(&self.search);
        let mut update_visible = 0;
        let mut installed_visible = 0;

        for app in &self.apps {
            if !query.is_empty() && !normalize(&app.name).contains(&query) {
                continue;
            }

            let mut display = app.clone();
            if let Some((path, metadata)) = &self.reloaded
                && path == &app.image_path
            {
                display.name.clone_from(&metadata.name);
                display.description.clone_from(&metadata.comment);
                display.version.clone_from(&metadata.version);
            }

            let has_update = self.updates.contains(&app.image_path);
            let action = if has_update {
                RowAction::Update
            } else {
                RowAction::Open
            };
            let single = self
                .single_update
                .as_ref()
                .filter(|update| update.path == app.image_path);
            let in_updates = has_update || single.is_some();
            let ListRow {
                row,
                single: single_row,
            } = Self::list_row(
                &display,
                action,
                single,
                &self.paths.metadata_cache_dir(),
                sender,
            );

            if self.page == Page::List
                && let Some(single_row) = single_row
            {
                self.single_row = Some(single_row);
            }

            if in_updates {
                update_visible += 1;
                widgets.updates_list.append(&row);
            } else {
                installed_visible += 1;
                widgets.installed_list.append(&row);
            }
        }

        let has_updates = self.has_updates();
        let sections = update_visible > 0;
        widgets.updates_section.set_visible(sections);
        widgets.installed_section.set_visible(installed_visible > 0);
        widgets.installed_header.set_visible(sections);

        let title = if sections {
            t("Applications")
        } else {
            t("Installed applications")
        };
        widgets.list_title.set_label(&title);

        widgets
            .list_stack
            .set_visible_child_name(if update_visible + installed_visible == 0 {
                "empty"
            } else {
                "list"
            });
        widgets.update_all_button.set_visible(has_updates);
    }

    /// Returns whether any installed app has a known update.
    fn has_updates(&self) -> bool {
        self.apps
            .iter()
            .any(|app| self.updates.contains(&app.image_path))
    }

    fn list_row(
        app: &InstalledApp,
        action: RowAction,
        single: Option<&SingleUpdate>,
        cache_dir: &Path,
        sender: &ComponentSender<Self>,
    ) -> ListRow {
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
        name.set_hexpand(true);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
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
        subtitle.set_hexpand(true);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle.add_css_class("dim-label");
        text.append(&subtitle);

        line.append(&text);

        let single_row = if let Some(update) = single {
            let progress_row = Self::single_update_row(update, &app.name);
            line.append(&progress_row.status);
            Some(progress_row)
        } else {
            let label = match action {
                RowAction::Update => t("Update"),
                RowAction::Open => t("Open"),
            };
            let button = gtk::Button::with_label(&label);
            button.set_valign(gtk::Align::Center);
            if action == RowAction::Update {
                button.add_css_class("suggested-action");
            }
            let sender = sender.clone();
            let path = app.image_path.clone();
            button.connect_clicked(move |_| match action {
                RowAction::Update => sender.input(Msg::StartUpdate(path.clone())),
                RowAction::Open => sender.input(Msg::Launch(path.clone())),
            });
            line.append(&button);
            None
        };

        row.set_child(Some(&line));
        ListRow {
            row,
            single: single_row,
        }
    }

    /// Builds the stack that replaces the update button while an app updates.
    fn single_update_row(update: &SingleUpdate, app_name: &str) -> SingleUpdateRow {
        let status = gtk::Stack::new();
        status.set_transition_type(gtk::StackTransitionType::Crossfade);
        status.set_transition_duration(120);
        status.set_size_request(24, 24);
        status.set_halign(gtk::Align::Center);
        status.set_valign(gtk::Align::Center);
        status.set_margin_end(6);

        let ring = util::CircularProgress::new(20);
        ring.set_fraction(update.fraction);
        ring.activate();
        status.add_named(ring.widget(), Some("progress"));

        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.set_pixel_size(16);
        check.set_valign(gtk::Align::Center);
        check.add_css_class("success");
        status.add_named(&check, Some("done"));

        match update.stage {
            SingleUpdateStage::Running => {
                status.set_visible_child_name("progress");
                status.set_tooltip_text(Some(&tf("Updating {name}...", &[("name", app_name)])));
            }
            SingleUpdateStage::Done => status.set_visible_child_name("done"),
        }

        SingleUpdateRow { status, ring }
    }

    fn rebuild_details(&mut self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        while let Some(child) = widgets.details.first_child() {
            widgets.details.remove(&child);
        }
        if self.page == Page::Details {
            self.single_row = None;
        }

        let Some(item) = &self.details else {
            return;
        };

        let appimage = match item {
            DetailsItem::Installed(app) => AppImage::open(&app.image_path).ok(),
            DetailsItem::Candidate(_) => None,
        };

        let arch = match item {
            DetailsItem::Installed(app) => appimage
                .as_ref()
                .map(AppImage::arch)
                .or_else(|| detect::elf_arch(&app.image_path).ok()),
            DetailsItem::Candidate(candidate) => Some(candidate.arch),
        };

        if arch.is_some_and(|arch| !arch.is_compatible_with_host()) {
            widgets.details.append(&architecture_banner());
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.set_margin_top(18);
        content.set_margin_bottom(24);
        content.set_margin_start(24);
        content.set_margin_end(24);

        let single_row = match item {
            DetailsItem::Installed(app) => {
                self.build_installed_details(app, appimage.as_ref(), &content, sender)
            }
            DetailsItem::Candidate(candidate) => {
                Self::build_candidate_details(candidate, &content, sender);
                None
            }
        };
        if self.page == Page::Details {
            self.single_row = single_row;
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
        appimage: Option<&AppImage>,
        content: &gtk::Box,
        sender: &ComponentSender<Self>,
    ) -> Option<SingleUpdateRow> {
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
        let update_source = if minimal {
            None
        } else {
            Some(update::source_info(
                &app.image_path,
                &self.config.update_manager(&app.image_path),
            ))
        };
        let mut pills = Vec::new();
        if !minimal {
            if let Some(version) = version {
                pills.push(version);
            }
            pills.push(size(app.size));
            if let Some(image) = appimage {
                pills.push(format!("AppImage Type. {}", image.image_type().code()));
                let payload = image.payload_kind();
                if payload != PayloadKind::Unknown {
                    pills.push(payload.label().to_owned());
                }
            }
            if let Some(source) = &update_source {
                pills.extend(update_source_pills(source));
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

        let mut single_row = None;
        if !minimal {
            if let Some(update) = self
                .single_update
                .as_ref()
                .filter(|update| update.path == app.image_path)
            {
                let row = Self::single_update_row(update, &name);
                actions.append(&row.status);
                single_row = Some(row);
            } else if self.updates.contains(&app.image_path) {
                let update = gtk::Button::with_label(&t("Update"));
                update.add_css_class("suggested-action");
                update.set_size_request(ACTION_BUTTON_WIDTH, -1);
                let sender_update = sender.clone();
                let path = app.image_path.clone();
                update.connect_clicked(move |_| {
                    sender_update.input(Msg::StartUpdate(path.clone()));
                });
                actions.append(&update);
            }
        }

        content.append(&actions);

        if minimal {
            return single_row;
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
        if matches!(update_source.as_ref(), Some(Ok(Some(_)))) {
            reload_list.append(&self.updates_toggle_row(app, sender));
        }
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
        let history = self.environment_history();
        content.append(&Self::env_group(
            app,
            &self.config,
            &history,
            sender,
            current_arguments,
        ));

        single_row
    }

    fn build_candidate_details(
        candidate: &CandidateFile,
        content: &gtk::Box,
        sender: &ComponentSender<Self>,
    ) {
        content.append(&details_header(
            &candidate.metadata.name,
            "",
            &util::candidate_icon(&candidate.metadata, util::DETAILS_ICON),
            &[],
        ));

        let actions = gtk::Box::new(gtk::Orientation::Vertical, 10);
        actions.set_halign(gtk::Align::Center);

        let launch = gtk::Button::with_label(&t("Launch"));
        launch.set_size_request(PREVIEW_BUTTON_WIDTH, CANDIDATE_BUTTON_HEIGHT);
        let sender_launch = sender.clone();
        let path_buf = candidate.path.clone();
        launch.connect_clicked(move |_| sender_launch.input(Msg::Launch(path_buf.clone())));
        actions.append(&launch);

        let install = gtk::Button::with_label(&t("Move to the app menu"));
        install.add_css_class("suggested-action");
        install.set_size_request(PREVIEW_BUTTON_WIDTH, CANDIDATE_BUTTON_HEIGHT);
        let sender_install = sender.clone();
        let path_buf = candidate.path.clone();
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
            && update::resolve(path, &config, None)
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
        config: &Config,
        history: &[String],
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
        let history_button = gtk::MenuButton::new();
        history_button.set_icon_name("document-open-recent-symbolic");
        history_button.set_direction(gtk::ArrowType::Up);
        history_button.set_valign(gtk::Align::Center);
        history_button.set_tooltip_text(Some(&t("Environment variable history")));
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
        header.append(&history_button);
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
            suspend_save: Rc::new(Cell::new(false)),
        };

        for entry in &app.env_variables {
            let (key, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            editor.add(key, value, EnvState::Enabled);
        }

        for entry in config.disabled_env(&app.image_path) {
            let (key, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            // A key that is also active was re-enabled outside the editor; the
            // desktop entry wins and the stale configuration entry is dropped
            // the next time the variables are saved.
            if app
                .env_variables
                .iter()
                .any(|variable| env_key(variable) == key)
            {
                continue;
            }
            editor.add(key, value, EnvState::Disabled);
        }
        editor.refresh();
        history_button.set_popover(Some(&environment_history_popover(&editor, history)));

        let editor_add = editor.clone();
        add.connect_clicked(move |_| {
            editor_add.add("", "", EnvState::Enabled);
            editor_add.refresh();
        });
        let editor_save = editor;
        save.connect_clicked(move |_| editor_save.save());

        group
    }

    fn rebuild_multi(&mut self, widgets: &Widgets, sender: &ComponentSender<Self>) {
        while let Some(child) = widgets.multi.first_child() {
            widgets.multi.remove(&child);
        }

        let content = gtk::Box::new(gtk::Orientation::Vertical, 20);
        content.set_margin_top(30);
        content.set_margin_bottom(10);
        content.set_margin_start(20);
        content.set_margin_end(20);

        match self.page {
            Page::MultiInstall => self.rebuild_multi_install(&content, sender),
            Page::MultiUpdate => self.rebuild_multi_update(&content, sender),
            Page::List | Page::Details => {}
        }

        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .child(&content)
            .build();
        widgets.multi.append(&clamp);
    }

    fn rebuild_multi_install(&self, content: &gtk::Box, sender: &ComponentSender<Self>) {
        let title = gtk::Label::new(Some(&t("Apps to integrate")));
        title.add_css_class("title-2");
        title.set_halign(gtk::Align::Start);
        content.append(&title);

        let conflicts = self
            .pending
            .iter()
            .filter(|candidate| conflict_app(&self.apps, &candidate.metadata).is_some())
            .count();
        if conflicts > 0 {
            let warning = gtk::Label::new(Some(&tf(
                "{count} of the selected AppImages have the same name as an app that is already integrated.",
                &[("count", &conflicts.to_string())],
            )));
            warning.add_css_class("dim-label");
            warning.set_halign(gtk::Align::Start);
            warning.set_wrap(true);
            content.append(&warning);
        }

        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);

        for candidate in &self.pending {
            list.append(&self.multi_install_row(candidate));
        }

        for ignored in &self.multi_ignored {
            list.append(&self.ignored_multi_row(ignored));
        }

        content.append(&list);

        if self.pending.is_empty() {
            let nothing =
                gtk::Label::new(Some(&t("None of the selected AppImages can be integrated")));
            nothing.add_css_class("dim-label");
            nothing.set_halign(gtk::Align::Center);
            nothing.set_wrap(true);
            content.append(&nothing);
        }

        let all = gtk::Button::with_label(&t("Move all to the app menu"));
        all.add_css_class("suggested-action");
        all.set_halign(gtk::Align::Center);
        all.set_sensitive(!self.install_running && !self.pending.is_empty());
        let sender = sender.clone();
        all.connect_clicked(move |_| sender.input(Msg::InstallAll));
        content.append(&all);
    }

    fn multi_install_row(&self, candidate: &CandidateFile) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(candidate.metadata.name.clone())
            .subtitle(candidate.path.to_string_lossy().into_owned())
            .build();
        row.add_prefix(&util::candidate_icon(&candidate.metadata, util::ROW_ICON));

        if conflict_app(&self.apps, &candidate.metadata).is_some() {
            let tooltip =
                t("There is already an app with the same name, how do you want to proceed?");
            row.add_suffix(&status_suffix(
                "dialog-warning-symbolic",
                &t("Name conflict"),
                Some(&tooltip),
                "warning",
            ));
        }

        row
    }

    fn ignored_multi_row(&self, ignored: &IgnoredCandidate) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(ignored.candidate.metadata.name.clone())
            .subtitle(ignored.candidate.path.to_string_lossy().into_owned())
            .build();
        row.add_prefix(&util::candidate_icon(
            &ignored.candidate.metadata,
            util::ROW_ICON,
        ));
        row.set_opacity(0.6);

        let (icon, text, tooltip, css) = match &ignored.reason {
            IgnoredReason::ForeignArchitecture => {
                let arch = ignored.candidate.arch.to_string();
                (
                    "dialog-warning-symbolic",
                    t("Incompatible architecture"),
                    tf(
                        "Built for {arch}; it will not be integrated",
                        &[("arch", &arch)],
                    ),
                    "warning",
                )
            }
            IgnoredReason::AlreadyInstalled(path) => {
                let version = self
                    .apps
                    .iter()
                    .find(|app| &app.image_path == path)
                    .and_then(|app| app.version.as_deref());
                let tooltip = version.map_or_else(
                    || t("This AppImage is already integrated"),
                    |version| {
                        tf(
                            "Version {version} is already integrated",
                            &[("version", version)],
                        )
                    },
                );
                (
                    "object-select-symbolic",
                    t("Already integrated"),
                    tooltip,
                    "success",
                )
            }
        };

        row.set_tooltip_text(Some(&tooltip));
        row.add_suffix(&status_suffix(icon, &text, Some(&tooltip), css));
        row
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the header, result summary and every row share the same status widgets"
    )]
    fn rebuild_multi_update(&mut self, content: &gtk::Box, sender: &ComponentSender<Self>) {
        self.multi_rows.clear();

        if self.multi_finished {
            let title = gtk::Label::new(Some(&t("Update results")));
            title.add_css_class("title-2");
            title.set_halign(gtk::Align::Start);
            content.append(&title);

            let cancelled = self.multi_cancelled.len();
            if cancelled > 0 {
                let warning = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                warning.set_halign(gtk::Align::Start);
                let icon = gtk::Image::from_icon_name("process-stop-symbolic");
                icon.add_css_class("warning");
                warning.append(&icon);
                let label = gtk::Label::new(Some(&t("Updates cancelled")));
                label.add_css_class("warning");
                label.set_wrap(true);
                warning.append(&label);
                content.append(&warning);
            }

            let failed = self.multi_failed.len();
            if failed > 0 {
                let summary = if failed == 1 {
                    t("1 update failed")
                } else {
                    tf("{count} updates failed", &[("count", &failed.to_string())])
                };
                let warning = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                warning.set_halign(gtk::Align::Start);
                let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
                icon.add_css_class("error");
                warning.append(&icon);
                let label = gtk::Label::new(Some(&summary));
                label.add_css_class("error");
                label.set_wrap(true);
                warning.append(&label);
                content.append(&warning);
            }
        } else {
            let cancelling = self
                .cancel_token
                .as_ref()
                .is_some_and(|token| token.load(Ordering::Relaxed));
            let label = if cancelling {
                t("Cancelling...")
            } else {
                t("Cancel update")
            };
            let cancel = gtk::Button::with_label(&label);
            cancel.set_halign(gtk::Align::Center);
            cancel.set_sensitive(!cancelling);
            let sender_cancel = sender.clone();
            cancel.connect_clicked(move |_| sender_cancel.input(Msg::CancelUpdate));
            content.append(&cancel);
        }

        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);

        for app in &self.update_queue {
            let path = &app.image_path;
            let done = self.multi_done.contains(path);
            let cancelled = self.multi_cancelled.contains(path);
            let failed = self.multi_failed.get(path);
            let active = self.multi_active.contains(path);
            let fraction = self.multi_fractions.get(path).copied().unwrap_or(0.0);

            let row = gtk::ListBoxRow::new();
            row.set_activatable(false);
            row.set_opacity(if done || cancelled || failed.is_some() || active {
                1.0
            } else {
                0.5
            });

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

            let error_label = gtk::Label::new(failed.map(String::as_str));
            error_label.add_css_class("error");
            error_label.set_halign(gtk::Align::Start);
            error_label.set_xalign(0.0);
            error_label.set_wrap(true);
            error_label.set_lines(2);
            error_label.set_max_width_chars(100);
            error_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            error_label.set_visible(failed.is_some());
            if let Some(error) = failed {
                error_label.set_tooltip_text(Some(error));
            }
            text.append(&error_label);

            line.append(&text);

            let status = gtk::Stack::new();
            status.set_transition_type(gtk::StackTransitionType::Crossfade);
            status.set_transition_duration(120);
            status.set_size_request(24, 24);
            status.set_halign(gtk::Align::Center);
            status.set_valign(gtk::Align::Center);
            status.set_margin_end(6);

            let ring = util::CircularProgress::new(20);
            ring.set_fraction(fraction);
            if active {
                ring.activate();
            }
            status.add_named(ring.widget(), Some("progress"));

            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_pixel_size(16);
            check.set_valign(gtk::Align::Center);
            check.add_css_class("success");
            status.add_named(&check, Some("done"));

            let error_icon = gtk::Image::from_icon_name("window-close-symbolic");
            error_icon.set_pixel_size(16);
            error_icon.set_valign(gtk::Align::Center);
            error_icon.add_css_class("error");
            status.add_named(&error_icon, Some("failed"));

            let cancelled_icon = gtk::Image::from_icon_name("process-stop-symbolic");
            cancelled_icon.set_pixel_size(16);
            cancelled_icon.set_valign(gtk::Align::Center);
            cancelled_icon.add_css_class("dim-label");
            status.add_named(&cancelled_icon, Some("cancelled"));

            if done {
                status.set_visible_child_name("done");
            } else if let Some(error) = failed {
                status.set_visible_child_name("failed");
                status.set_tooltip_text(Some(error));
            } else if cancelled {
                status.set_visible_child_name("cancelled");
                status.set_tooltip_text(Some(&t("Update cancelled")));
            } else {
                status.set_visible_child_name("progress");
                if active {
                    status
                        .set_tooltip_text(Some(&tf("Updating {name}...", &[("name", &app.name)])));
                }
            }

            line.append(&status);
            row.set_child(Some(&line));
            list.append(&row);

            self.multi_rows.insert(
                app.image_path.clone(),
                MultiUpdateRow {
                    row,
                    status,
                    ring,
                    error: error_label,
                },
            );
        }

        content.append(&list);

        if self.multi_finished {
            let close = gtk::Button::with_label(&t("Close"));
            close.set_halign(gtk::Align::Center);
            let sender_close = sender.clone();
            close.connect_clicked(move |_| sender_close.input(Msg::ShowList));
            content.append(&close);
        }
    }

    /// Prompts for a restart after the interface scale changes.
    fn show_ui_scale_restart_dialog(
        preferences: &glib::WeakRef<adw::PreferencesWindow>,
        main: &glib::WeakRef<adw::ApplicationWindow>,
        sender: &ComponentSender<Self>,
        dialog_open: &Rc<Cell<bool>>,
    ) {
        if dialog_open.get() {
            return;
        }

        let preferences = preferences.upgrade();
        let main = main.upgrade();
        let parent = preferences
            .as_ref()
            .filter(|window| window.is_visible())
            .map(|window| window.clone().upcast::<gtk::Window>())
            .or_else(|| {
                main.as_ref()
                    .map(|window| window.clone().upcast::<gtk::Window>())
            });
        let Some(parent) = parent else {
            return;
        };

        dialog_open.set(true);
        let dialog = adw::MessageDialog::new(
            Some(&parent),
            Some(&t("Restart required")),
            Some(&t("Restart Gear LeverR to apply the new interface scale.")),
        );
        dialog.add_response("later", &t("Later"));
        dialog.add_response("restart", &t("Restart"));
        dialog.set_response_appearance("restart", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("restart"));
        dialog.set_close_response("later");

        let sender = sender.clone();
        let dialog_open = Rc::clone(dialog_open);
        dialog.connect_response(None, move |dialog: &adw::MessageDialog, response: &str| {
            dialog_open.set(false);
            if response == "restart" {
                sender.input(Msg::RestartApp);
            }
            dialog.close();
        });
        dialog.present();
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

        let appearance = adw::PreferencesGroup::new();
        appearance.set_title(&t("Appearance"));

        let scale_row = adw::ActionRow::builder()
            .title(t("Interface scale"))
            .subtitle(t("Increase the size of text and buttons"))
            .build();
        scale_row.add_prefix(&gtk::Image::from_icon_name(
            "preferences-desktop-font-symbolic",
        ));

        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 100.0, 200.0, 10.0);
        scale.set_digits(0);
        scale.set_draw_value(false);
        scale.set_size_request(180, -1);
        scale.set_valign(gtk::Align::Center);
        scale.set_value(f64::from(self.settings.values().ui_scale));

        let spin = gtk::SpinButton::with_range(100.0, 200.0, 10.0);
        spin.set_digits(0);
        spin.set_numeric(true);
        spin.set_width_chars(4);
        spin.set_max_width_chars(4);
        spin.set_valign(gtk::Align::Center);
        spin.set_value(f64::from(self.settings.values().ui_scale));

        let percent = gtk::Label::new(Some("%"));
        percent.add_css_class("dim-label");

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.set_valign(gtk::Align::Center);
        controls.append(&scale);
        controls.append(&spin);
        controls.append(&percent);
        scale_row.add_suffix(&controls);
        appearance.add(&scale_row);

        let applied_scale = self.applied_ui_scale;
        let sender_scale = sender.clone();
        let generation = Rc::new(Cell::new(0_u64));
        let dialog_open = Rc::new(Cell::new(false));
        let pointer_down = Rc::new(Cell::new(false));
        let pending_restart = Rc::new(Cell::new(false));

        let show_restart: Rc<dyn Fn()> = {
            let preferences = window.downgrade();
            let main = widgets.window.downgrade();
            let sender = sender.clone();
            let dialog_open = Rc::clone(&dialog_open);
            Rc::new(move || {
                Self::show_ui_scale_restart_dialog(&preferences, &main, &sender, &dialog_open);
            })
        };

        let pointer_down_spin = Rc::clone(&pointer_down);
        let pending_restart_spin = Rc::clone(&pending_restart);
        let show_restart_spin = Rc::clone(&show_restart);
        let scale_ref = scale.downgrade();
        let spin_timer_ref = spin.downgrade();
        spin.connect_value_changed(move |spin| {
            let value = spin.value();
            if let Some(scale) = scale_ref.upgrade()
                && (scale.value() - value).abs() > f64::EPSILON
            {
                scale.set_value(value);
            }

            let percent = u32::try_from(spin.value_as_int()).unwrap_or(100);
            sender_scale.input(Msg::UiScaleChanged(percent));

            let current = generation.get().wrapping_add(1);
            generation.set(current);
            pending_restart_spin.set(false);

            if percent == applied_scale {
                return;
            }

            let generation_timer = Rc::clone(&generation);
            let dialog_open_timer = Rc::clone(&dialog_open);
            let pointer_down_timer = Rc::clone(&pointer_down_spin);
            let pending_restart_timer = Rc::clone(&pending_restart_spin);
            let show_restart = Rc::clone(&show_restart_spin);
            let spin_timer_ref = spin_timer_ref.clone();

            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                if generation_timer.get() != current || dialog_open_timer.get() {
                    return;
                }

                if pointer_down_timer.get() {
                    pending_restart_timer.set(true);
                    return;
                }

                let at_applied_scale = spin_timer_ref.upgrade().is_some_and(|spin| {
                    u32::try_from(spin.value_as_int()).unwrap_or(100) == applied_scale
                });
                if at_applied_scale {
                    return;
                }

                show_restart();
            });
        });

        let spin_ref = spin.downgrade();
        scale.connect_value_changed(move |scale| {
            let value = scale.value();
            if let Some(spin) = spin_ref.upgrade()
                && (spin.value() - value).abs() > f64::EPSILON
            {
                spin.set_value(value);
            }
        });

        let pointer_down_release = Rc::clone(&pointer_down);
        let pending_restart_release = Rc::clone(&pending_restart);
        let show_restart_release = Rc::clone(&show_restart);
        let spin_release_ref = spin.downgrade();
        // GtkScale owns its drag gesture, so another gesture would lose the
        // sequence. This controller only observes the pointer and lets the
        // scale keep dragging; the prompt waits until the button is released.
        let pointer_controller = gtk::EventControllerLegacy::new();
        pointer_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        pointer_controller.connect_event(move |_, event| {
            match event.event_type() {
                gdk::EventType::ButtonPress | gdk::EventType::TouchBegin => {
                    pointer_down_release.set(true);
                    pending_restart_release.set(false);
                }
                gdk::EventType::ButtonRelease
                | gdk::EventType::TouchEnd
                | gdk::EventType::TouchCancel
                | gdk::EventType::GrabBroken => {
                    pointer_down_release.set(false);
                    if pending_restart_release.take() {
                        let at_applied_scale = spin_release_ref.upgrade().is_some_and(|spin| {
                            u32::try_from(spin.value_as_int()).unwrap_or(100) == applied_scale
                        });
                        if !at_applied_scale {
                            show_restart_release();
                        }
                    }
                }
                _ => {}
            }

            glib::Propagation::Proceed
        });
        controls.add_controller(pointer_controller);

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
        updates.add(&self.switch_row(
            &t("Use zsync delta updates"),
            Some(&t(
                "Download only changed blocks when the source provides zsync data.\nOn very low-resource computers you may prefer to disable it: updates then download the full AppImage and skip block checksum scanning.",
            )),
            keys::USE_ZSYNC_DELTA,
            self.settings.values().use_zsync_delta,
            sender,
        ));
        updates.add(&Self::parallel_row(
            &t("Parallel downloads"),
            &t(
                "Updates downloaded at the same time. Set it to 1 on slow or metered connections; installing updates is always sequential.",
            ),
            self.settings.values().max_parallel_downloads,
            MAX_PARALLEL_DOWNLOADS,
            Msg::SetParallelDownloads,
            sender,
        ));
        updates.add(&Self::parallel_row(
            &t("Parallel update checks"),
            &t(
                "Apps checked for updates at the same time. Lower it when the update source limits concurrent requests.",
            ),
            self.settings.values().max_parallel_checks,
            MAX_PARALLEL_CHECKS,
            Msg::SetParallelChecks,
            sender,
        ));

        let github = adw::PreferencesGroup::new();
        github.set_title(&t("GitHub"));
        github.set_description(Some(&t(
            "Authenticate with GitHub to raise the API rate limit from 60 to 5,000 requests per hour",
        )));

        let token_store = TokenStore::new(self.settings.clone());
        let mut status = token_store.status();
        let lookup = token_store.lookup_interactive();
        if matches!(lookup, TokenLookup::Missing) && status != TokenStatus::None {
            // A previous build could mark a token as valid after testing it
            // without saving it. If no token exists now, the status is stale.
            let _ = token_store.mark_none();
            status = TokenStatus::None;
        }
        if status == TokenStatus::None
            && matches!(lookup, TokenLookup::Found(_) | TokenLookup::Locked)
        {
            let _ = token_store.mark_unchecked();
            status = TokenStatus::Unchecked;
        }
        let configured = status != TokenStatus::None
            || matches!(lookup, TokenLookup::Found(_) | TokenLookup::Locked);
        let current_token = match &lookup {
            TokenLookup::Found(token) => Some(token.as_str()),
            TokenLookup::Locked | TokenLookup::Unavailable | TokenLookup::Missing => None,
        };
        let token_missing = configured && current_token.is_none();

        let token_row = adw::PasswordEntryRow::new();
        token_row.set_title(&if token_missing {
            t("Personal access token (configured)")
        } else {
            t("Personal access token")
        });
        token_row.add_prefix(&gtk::Image::from_icon_name("dialog-password-symbolic"));
        if let Some(token) = current_token {
            token_row.set_text(token);
        }

        let apply = gtk::Button::from_icon_name("object-select-symbolic");
        apply.set_valign(gtk::Align::Center);
        apply.add_css_class("flat");
        apply.add_css_class("success");
        apply.set_tooltip_text(Some(&t("Apply token")));

        let clear = gtk::Button::from_icon_name("edit-clear-symbolic");
        clear.set_valign(gtk::Align::Center);
        clear.add_css_class("flat");
        clear.set_tooltip_text(Some(&t("Clear token")));

        let status_row = adw::ActionRow::builder().title(t("Test token")).build();
        status_row.add_prefix(&gtk::Image::from_icon_name("object-select-symbolic"));
        status_row.set_subtitle(&initial_github_status(&token_store, status, &lookup));

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        let test = gtk::Button::with_label(&t("Test"));
        test.set_valign(gtk::Align::Center);
        test.add_css_class("flat");
        test.set_sensitive(current_token.is_some_and(valid_token));

        let store_apply = token_store.clone();
        let row_apply = token_row.clone();
        let status_apply = status_row.clone();
        apply.connect_clicked(move |_| {
            let _ = save_github_token(&row_apply, &status_apply, &store_apply);
        });

        let store_clear = token_store.clone();
        let row_clear = token_row.clone();
        let status_clear = status_row.clone();
        clear.connect_clicked(move |_| {
            clear_github_token(&row_clear, &status_clear, &store_clear);
        });

        let store_test = token_store.clone();
        let store_enter = token_store;
        let status_enter = status_row.clone();
        token_row.connect_entry_activated(move |row| {
            let _ = save_github_token(row, &status_enter, &store_enter);
        });

        let test_row = token_row.clone();
        let test_status = status_row.clone();
        let test_button = test.clone();
        let test_spinner = spinner.clone();
        let sender_test = sender.clone();
        test.connect_clicked(move |_| {
            let token = test_row.text().trim().to_owned();
            if !valid_token(&token) {
                test_status.set_subtitle(&t("Enter a valid token"));
                return;
            }
            if !save_github_token(&test_row, &test_status, &store_test) {
                return;
            }

            start_token_test(
                token,
                &test_status,
                &test_button,
                &test_spinner,
                &sender_test,
            );
        });

        let test_for_change = test.clone();
        token_row.connect_changed(move |row| {
            test_for_change.set_sensitive(valid_token(row.text().as_str()));
        });

        token_row.add_suffix(&apply);
        token_row.add_suffix(&clear);

        status_row.add_suffix(&spinner);
        status_row.add_suffix(&test);

        let help_row = adw::ActionRow::builder()
            .title(t("Create a token on GitHub"))
            .subtitle(t(
                "Fine-grained token with read-only public repo access is sufficient",
            ))
            .activatable(true)
            .build();
        help_row.add_prefix(&gtk::Image::from_icon_name("help-about-symbolic"));
        help_row.add_suffix(&gtk::Image::from_icon_name("external-link-symbolic"));
        let sender_help = sender.clone();
        help_row.connect_activated(move |_| {
            sender_help.input(Msg::OpenUrl(
                "https://github.com/settings/tokens?type=beta".to_owned(),
            ));
        });

        github.add(&token_row);
        github.add(&status_row);
        github.add(&help_row);

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

        let system = Self::system_group(sender);

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
        page.add(&appearance);
        page.add(&updates);
        page.add(&github);
        page.add(&files);
        page.add(&naming);
        page.add(&system);
        page.add(&debugging);
        window.add(&page);
        window.present();
    }

    /// Builds the system status group shown in Preferences.
    fn system_group(sender: &ComponentSender<Self>) -> adw::PreferencesGroup {
        let report = FuseReport::detect();
        let group = adw::PreferencesGroup::new();
        group.set_title(&t("System"));

        let mut mount_fixes = Vec::new();
        if !report.has_device() {
            mount_fixes.push("sudo modprobe fuse".to_owned());
        }
        if !report.has_fuse2_helper()
            && !report.has_fuse3_helper()
            && let Some(command) = FuseComponent::Mount.install_command()
        {
            mount_fixes.push(command);
        }
        let mount_fix = (!mount_fixes.is_empty()).then(|| mount_fixes.join("\n"));
        let fuse3_fix = if report.has_device() {
            FuseComponent::Mount.install_command()
        } else {
            None
        };
        let legacy_fix = if report.has_device() {
            FuseComponent::Legacy.install_command()
        } else {
            None
        };

        let mount_status = if report.can_mount() {
            SystemStatus::Detected
        } else {
            SystemStatus::Missing
        };
        let device_status = if report.has_device() {
            SystemStatus::Detected
        } else {
            SystemStatus::Missing
        };
        let fuse3_status = if report.has_fuse3_helper() {
            SystemStatus::Detected
        } else {
            SystemStatus::Missing
        };
        let fuse2_status = if report.legacy_ready() {
            SystemStatus::Detected
        } else {
            SystemStatus::Missing
        };

        group.add(&Self::status_row(
            &t("Mounting"),
            &t("Gear LeverR needs FUSE to mount the AppImage instead of extracting it."),
            mount_status,
            mount_fix,
        ));
        group.add(&Self::status_row(
            &t("FUSE device"),
            &t("The kernel device AppImages use to mount their filesystem."),
            device_status,
            None,
        ));
        group.add(&Self::status_row(
            &t("FUSE 3"),
            &t("The fusermount3 helper. Gear LeverR can use it or fusermount."),
            fuse3_status,
            fuse3_fix,
        ));
        group.add(&Self::status_row(
            &t("FUSE 2"),
            &t("Needed by AppImages built with the older runtime. Gear LeverR does not use it."),
            fuse2_status,
            legacy_fix,
        ));
        group.add(&Self::running_mode_row());

        let docs = adw::ActionRow::builder()
            .title(t("FUSE documentation"))
            .subtitle(t("How to install FUSE on your distribution"))
            .activatable(true)
            .build();
        docs.add_prefix(&gtk::Image::from_icon_name("help-about-symbolic"));
        docs.add_suffix(&gtk::Image::from_icon_name("external-link-symbolic"));
        let sender_docs = sender.clone();
        docs.connect_activated(move |_| {
            sender_docs.input(Msg::OpenUrl(FUSE_DOCS_URL.to_owned()));
        });
        group.add(&docs);

        group
    }

    /// Builds a row with a detected/not-detected status and an optional fix.
    fn status_row(
        title: &str,
        subtitle: &str,
        status: SystemStatus,
        fix: Option<String>,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder().title(title).build();
        row.set_activatable(false);
        row.add_prefix(&gtk::Image::from_icon_name(
            if status == SystemStatus::Detected {
                "emblem-ok-symbolic"
            } else {
                "dialog-warning-symbolic"
            },
        ));

        let mut subtitle = subtitle.to_owned();
        if status == SystemStatus::Missing
            && let Some(command) = fix
        {
            subtitle.push('\n');
            subtitle.push_str(&command);
        }
        row.set_subtitle(&subtitle);
        row.set_subtitle_lines(0);

        let status_text = if status == SystemStatus::Detected {
            t("Detected")
        } else {
            t("Not detected")
        };
        let status = gtk::Label::new(Some(&status_text));
        status.add_css_class("dim-label");
        status.set_valign(gtk::Align::Center);
        row.add_suffix(&status);

        row
    }

    /// Builds the row that reports how the current process was started.
    fn running_mode_row() -> adw::ActionRow {
        let (status, subtitle) = match system::running_mode() {
            RunningMode::Mounted => (
                t("Mounted"),
                t("The AppImage filesystem is mounted through FUSE."),
            ),
            RunningMode::Extracted => (
                t("Extracted"),
                t("Gear LeverR extracted the AppImage to a temporary directory."),
            ),
            RunningMode::NotAnAppImage => (
                t("Not an AppImage"),
                t("Gear LeverR is not running from an AppImage."),
            ),
            _ => (
                t("Unknown"),
                t("Could not check how Gear LeverR was started."),
            ),
        };

        let row = adw::ActionRow::builder()
            .title(t("Running from"))
            .subtitle(subtitle)
            .build();
        row.set_activatable(false);
        row.add_prefix(&gtk::Image::from_icon_name(
            "application-x-executable-symbolic",
        ));
        let status = gtk::Label::new(Some(&status));
        status.add_css_class("dim-label");
        status.set_valign(gtk::Align::Center);
        row.add_suffix(&status);

        row
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

    /// Builds a row with a numeric spinner for a parallel-task setting.
    fn parallel_row(
        title: &str,
        subtitle: &str,
        value: usize,
        max: usize,
        message: fn(usize) -> Msg,
        sender: &ComponentSender<Self>,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        row.set_activatable(false);

        let max = f64::from(u32::try_from(max).unwrap_or(1));
        let spin = gtk::SpinButton::with_range(1.0, max, 1.0);
        spin.set_digits(0);
        spin.set_numeric(true);
        spin.set_width_chars(2);
        spin.set_valign(gtk::Align::Center);
        spin.set_value(f64::from(u32::try_from(value).unwrap_or(1)));

        let sender = sender.clone();
        spin.connect_value_changed(move |spin| {
            let value = usize::try_from(spin.value_as_int()).unwrap_or(1);
            sender.input(message(value));
        });

        row.add_suffix(&spin);
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
            .comments(t("<b>About</b>\nAn AppImage manager inspired by Gear Lever, but an independent project: it shares neither its code, its name nor its icon.\n\n<b>Built with</b>\nRust, GTK 4, libadwaita, Relm4, backhand, ureq and zsync-rs (MIT)."))
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
        scope: ConflictDialogScope,
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

        let cancel_label = match scope {
            ConflictDialogScope::Single => t("Cancel"),
            ConflictDialogScope::Batch => t("Skip"),
        };
        let cancel = gtk::Button::with_label(&cancel_label);
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
            let message = match scope {
                ConflictDialogScope::Single => Msg::InstallWithConflict {
                    path: replace_path.clone(),
                    replace: true,
                },
                ConflictDialogScope::Batch => Msg::BatchConflictDecision {
                    path: replace_path.clone(),
                    decision: BatchConflictDecision::Replace,
                },
            };
            sender_replace.input(message);
            dialog_replace.close();
        });

        let sender_keep = sender.clone();
        let dialog_keep = dialog.clone();
        let keep_path = path.clone();
        keep.connect_clicked(move |_| {
            let message = match scope {
                ConflictDialogScope::Single => Msg::InstallWithConflict {
                    path: keep_path.clone(),
                    replace: false,
                },
                ConflictDialogScope::Batch => Msg::BatchConflictDecision {
                    path: keep_path.clone(),
                    decision: BatchConflictDecision::KeepBoth,
                },
            };
            sender_keep.input(message);
            dialog_keep.close();
        });

        let sender_skip = sender.clone();
        let dialog_skip = dialog.clone();
        let skip_path = path;
        cancel.connect_clicked(move |_| {
            if scope == ConflictDialogScope::Batch {
                sender_skip.input(Msg::BatchConflictDecision {
                    path: skip_path.clone(),
                    decision: BatchConflictDecision::Skip,
                });
            }
            dialog_skip.close();
        });

        dialog.present();
    }

    fn confirm_batch_conflict(widgets: &Widgets, count: usize, sender: &ComponentSender<Self>) {
        let dialog = adw::MessageDialog::new(
            Some(&widgets.window),
            Some(&t("Some apps are already installed")),
            Some(&tf(
                "{count} of the selected AppImages have the same name as an app that is already integrated.",
                &[("count", &count.to_string())],
            )),
        );

        let buttons = gtk::Box::new(gtk::Orientation::Vertical, 8);
        buttons.set_margin_top(6);

        let replace_all = gtk::Button::with_label(&t("Replace all"));
        replace_all.set_hexpand(true);

        let keep_all = gtk::Button::with_label(&t("Keep all"));
        keep_all.set_hexpand(true);

        let ask = gtk::Button::with_label(&t("Ask for each"));
        ask.add_css_class("suggested-action");
        ask.set_hexpand(true);

        let cancel = gtk::Button::with_label(&t("Cancel all"));
        cancel.add_css_class("destructive-action");
        cancel.set_hexpand(true);

        buttons.append(&replace_all);
        buttons.append(&keep_all);
        buttons.append(&ask);
        buttons.append(&cancel);
        dialog.set_extra_child(Some(&buttons));

        let sender_replace = sender.clone();
        let dialog_replace = dialog.clone();
        replace_all.connect_clicked(move |_| {
            sender_replace.input(Msg::BatchPolicy(BatchConflictPolicy::ReplaceAll));
            dialog_replace.close();
        });

        let sender_keep = sender.clone();
        let dialog_keep = dialog.clone();
        keep_all.connect_clicked(move |_| {
            sender_keep.input(Msg::BatchPolicy(BatchConflictPolicy::KeepAll));
            dialog_keep.close();
        });

        let sender_ask = sender.clone();
        let dialog_ask = dialog.clone();
        ask.connect_clicked(move |_| {
            sender_ask.input(Msg::BatchPolicy(BatchConflictPolicy::AskEach));
            dialog_ask.close();
        });

        let sender_cancel = sender.clone();
        let dialog_cancel = dialog.clone();
        cancel.connect_clicked(move |_| {
            sender_cancel.input(Msg::BatchConflictCancelled);
            dialog_cancel.close();
        });

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

    fn launch(
        app: &InstalledApp,
        sender: &ComponentSender<Self>,
        window_after_launch: WindowAfterLaunch,
    ) {
        let desktop_id = app.desktop_id.clone();
        let image_path = app.image_path.clone();
        let sender = sender.clone();

        // gtk-launch returns immediately once the app is started, so waiting for
        // it does not block anything the user can see.
        std::thread::spawn(move || {
            let launched = launch::command("gtk-launch")
                .arg(&desktop_id)
                .status()
                .is_ok_and(|status| status.success());

            let launched = launched || launch::command(&image_path).spawn().is_ok();

            if launched && window_after_launch == WindowAfterLaunch::Close {
                sender.input(Msg::CloseAfterLaunch);
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

    /// Restarts the program that is running Gear LeverR.
    fn restart_self(&self, widgets: &Widgets) {
        let program = running_appimage()
            .or_else(|| {
                // Installing itself can rename the running image, so `$APPIMAGE`
                // may point to a path that no longer exists. The desktop entry
                // already points at the canonical copy.
                std::env::var_os("APPIMAGE").and_then(|_| {
                    self.apps
                        .iter()
                        .find(|app| app.self_entry)
                        .map(|app| app.image_path.clone())
                })
            })
            .or_else(|| std::env::current_exe().ok())
            .or_else(|| {
                self.apps
                    .iter()
                    .find(|app| app.self_entry)
                    .map(|app| app.image_path.clone())
            });

        let Some(program) = program else {
            Self::toast(widgets, &t("Cannot restart Gear LeverR"));
            return;
        };

        let _ = Self::restart_into(widgets, &program, None);
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

    fn minimal_ui(widgets: &Widgets) {
        widgets.minimal_preview.set(true);
        widgets.left_button.set_visible(false);
        widgets.menu_button.set_visible(false);
        widgets.search_button.set_visible(false);
        widgets.title.set_visible(false);
        widgets.window.unmaximize();
        widgets.window.set_resizable(false);

        // Measure at the width the window will actually get, so text wrapping
        // on a narrow monitor is part of the height calculation.
        let (width, _) = fit_window_size(PREVIEW_WINDOW_WIDTH, 0);
        let header_height = widgets.header.measure(gtk::Orientation::Vertical, width).1;
        let details_height = widgets.details.measure(gtk::Orientation::Vertical, width).1;
        let content_height = header_height + details_height + PREVIEW_WINDOW_EXTRA_HEIGHT;
        let (width, height) = fit_window_size(width, content_height);
        widgets.window.set_default_size(width, height);
    }

    fn show_progress(widgets: &Widgets) {
        widgets.progress.set_visible(true);
    }

    fn hide_progress(widgets: &Widgets) {
        widgets.progress.set_visible(false);
    }

    fn toast(widgets: &Widgets, message: &str) {
        widgets.toast.add_toast(adw::Toast::new(message));
    }

    /// Shows an error toast long enough to read it.
    fn error_toast(widgets: &Widgets, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(ERROR_TOAST_TIMEOUT_SECS);
        widgets.toast.add_toast(toast);
    }

    /// Shows toasts for ordinary failures and a dialog for ISO9660 hints.
    fn show_load_failures(widgets: &Widgets, failures: Vec<LoadFailure>) {
        let (iso9660, other): (Vec<_>, Vec<_>) = failures
            .into_iter()
            .partition(|failure| failure.kind == LoadFailureKind::Iso9660Extractor);

        // Only the first toast gets the longer timeout; queueing one long
        // toast per file would keep the overlay busy for minutes.
        for (index, failure) in other.into_iter().enumerate() {
            if index == 0 {
                Self::error_toast(widgets, &failure.message);
            } else {
                Self::toast(widgets, &failure.message);
            }
        }

        if !iso9660.is_empty() {
            Self::show_iso9660_dialog(widgets, &iso9660);
        }
    }

    /// Explains that an ISO9660 payload needs 7-Zip and lets the user copy the
    /// package-manager command.
    fn show_iso9660_dialog(widgets: &Widgets, failures: &[LoadFailure]) {
        use relm4::gtk::gdk::prelude::*;

        let mut details = String::new();
        for (index, failure) in failures.iter().enumerate() {
            if index > 0 {
                details.push_str("\n\n");
            }
            details.push_str(&failure.message);
            details.push('\n');
            details.push_str(&failure.path.display().to_string());
        }

        let label = gtk::Label::new(Some(&details));
        label.set_wrap(true);
        label.set_selectable(true);
        label.set_xalign(0.0);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.append(&label);

        if let Some(command) = system::seven_zip_install_command() {
            let entry = gtk::Entry::new();
            entry.set_text(&command);
            entry.set_editable(false);
            entry.set_can_focus(true);
            entry.set_hexpand(true);

            let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
            copy.set_tooltip_text(Some(&t("Copy command")));
            let display = relm4::gtk::prelude::WidgetExt::display(&widgets.window);
            let clipboard = display.clipboard();
            let command_for_copy = command;
            copy.connect_clicked(move |_| {
                clipboard.set_text(&command_for_copy);
            });

            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.append(&entry);
            row.append(&copy);
            content.append(&row);
        }

        let dialog = adw::MessageDialog::new(
            Some(&widgets.window),
            Some(&t("Cannot read this AppImage")),
            None,
        );
        dialog.set_extra_child(Some(&content));
        dialog.add_response("close", &t("Close"));
        dialog.set_close_response("close");
        dialog.connect_response(None, |dialog: &adw::MessageDialog, _response| {
            dialog.close();
        });
        dialog.present();
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the shell is assembled once, in one place"
    )]
    fn build_shell(root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) -> Widgets {
        let header = adw::HeaderBar::new();

        let title = gtk::Label::new(Some(APP_NAME));
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

        let updates_progress = gtk::ProgressBar::new();
        updates_progress.set_show_text(false);
        updates_progress.set_visible(false);
        updates_progress.set_hexpand(true);
        updates_progress.add_css_class("update-progress");

        let updates_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        updates_box.append(&updates_button);
        updates_box.append(&updates_progress);
        title_row.append(&updates_box);

        let update_all_button = gtk::Button::with_label(&t("Update all"));
        update_all_button.add_css_class("suggested-action");
        update_all_button.set_visible(false);
        let sender_update_all = sender.clone();
        update_all_button.connect_clicked(move |_| sender_update_all.input(Msg::StartAllUpdates));
        title_row.append(&update_all_button);

        let updates_generation = Rc::new(Cell::new(0_u64));

        let updates_list = gtk::ListBox::new();
        let installed_list = gtk::ListBox::new();

        for listbox in [&updates_list, &installed_list] {
            listbox.add_css_class("boxed-list");
            listbox.set_selection_mode(gtk::SelectionMode::None);
            let sender_row = sender.clone();
            listbox.connect_row_activated(move |_, row| {
                sender_row.input(Msg::OpenPath(PathBuf::from(row.widget_name())));
            });
        }

        let updates_section = gtk::Box::new(gtk::Orientation::Vertical, 6);
        updates_section.append(&section_header(&t("Updates")));
        updates_section.append(&updates_list);
        updates_section.set_visible(false);

        let installed_header = section_header(&t("Installed"));
        installed_header.set_visible(false);

        let installed_section = gtk::Box::new(gtk::Orientation::Vertical, 6);
        installed_section.append(&installed_header);
        installed_section.append(&installed_list);

        let list_sections = gtk::Box::new(gtk::Orientation::Vertical, 18);
        list_sections.append(&updates_section);
        list_sections.append(&installed_section);

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
        list_stack.add_named(&list_sections, Some("list"));
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

        let progress_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        progress_row.set_margin_start(12);
        progress_row.set_margin_end(12);
        progress_row.set_margin_bottom(6);
        progress_row.append(&progress);

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
        let minimal_preview = Rc::new(Cell::new(false));
        let window_close = root.clone();
        let preview_close = Rc::clone(&minimal_preview);
        root.connect_close_request(move |window| {
            // The minimal preview reuses the main window, so its size and
            // maximized state must not replace the last normal window state.
            if !preview_close.get() {
                if !window.is_maximized() {
                    let width = window.default_width();
                    let height = window.default_height();
                    if width > 0 && height > 0 {
                        settings.store_window_size(width, height);
                    }
                }
                settings.store_boolean(keys::IS_MAXIMIZED, window.is_maximized());
            }
            let _ = fs::remove_dir_all(&temp);
            glib::Propagation::Proceed
        });

        Widgets {
            window: window_close,
            header,
            title,
            left_button,
            search_button,
            menu_button,
            search_bar,
            stack,
            toast,
            updates_section,
            updates_list,
            installed_section,
            installed_header,
            installed_list,
            list_title,
            list_stack,
            updates_button,
            updates_progress,
            updates_generation,
            update_all_button,
            details,
            multi,
            multi_progress,
            progress,
            drop_revealer,
            minimal_preview,
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
    /// Prevents the checkbox autosave while the editor applies history.
    suspend_save: Rc<Cell<bool>>,
}

struct EnvRow {
    id: u64,
    key: gtk::Entry,
    value: gtk::Entry,
    enabled: gtk::CheckButton,
}

/// Whether an environment variable is written to the desktop entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvState {
    Enabled,
    Disabled,
}

impl EnvEditor {
    fn add(&self, key: &str, value: &str, state: EnvState) {
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

        let check = gtk::CheckButton::new();
        check.set_active(state == EnvState::Enabled);
        check.set_valign(gtk::Align::Center);
        check.set_tooltip_text(Some(&t("Enable or disable this environment variable")));

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(12);
        row.set_margin_end(12);
        row.append(&key_entry);
        row.append(&gtk::Label::new(Some("=")));
        row.append(&value_entry);
        row.append(&delete);
        row.append(&check);

        self.container.append(&row);
        self.rows.borrow_mut().push(EnvRow {
            id,
            key: key_entry.clone(),
            value: value_entry.clone(),
            enabled: check.clone(),
        });

        let editor = self.clone();
        key_entry.connect_changed(move |_| editor.refresh());
        let editor = self.clone();
        value_entry.connect_changed(move |_| editor.refresh());
        let editor = self.clone();
        check.connect_toggled(move |_| {
            editor.refresh();
            if !editor.suspend_save.get() {
                editor.save();
            }
        });
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

            let opacity = if row.enabled.is_active() {
                1.0
            } else {
                DISABLED_ENV_OPACITY
            };
            row.key.set_opacity(opacity);
            row.value.set_opacity(opacity);

            if !unique {
                valid = false;
            }
        }

        self.save.set_sensitive(valid);
    }

    fn save(&self) {
        let rows: Vec<(String, String, EnvState)> = self
            .rows
            .borrow()
            .iter()
            .map(|row| {
                let state = if row.enabled.is_active() {
                    EnvState::Enabled
                } else {
                    EnvState::Disabled
                };
                (
                    row.key.text().trim().to_owned(),
                    row.value.text().trim().to_owned(),
                    state,
                )
            })
            .collect();
        let (env, disabled_env) = partition_environment(&rows);

        let arguments = self.arguments.borrow().clone();
        self.sender.input(Msg::SaveExec {
            path: self.path.clone(),
            env,
            disabled_env,
            arguments,
        });
    }

    /// Applies a history entry to the editor without saving it.
    ///
    /// An existing key keeps its row and gets the remembered value; an unknown
    /// key appends a new active row. The user still has to press Save.
    fn apply_history(&self, key: &str, value: &str) {
        if key.is_empty() {
            return;
        }

        let existing = {
            let rows = self.rows.borrow();
            rows.iter()
                .find(|row| row.key.text().trim() == key)
                .map(|row| (row.value.clone(), row.enabled.clone()))
        };

        match existing {
            Some((value_entry, check)) => {
                let suspended = self.suspend_save.replace(true);
                if value_entry.text().trim() != value {
                    value_entry.set_text(value);
                }
                if !check.is_active() {
                    check.set_active(true);
                }
                self.suspend_save.set(suspended);
            }
            None => self.add(key, value, EnvState::Enabled),
        }

        self.refresh();
    }
}

/// Returns the key of a `KEY=value` environment entry.
fn env_key(entry: &str) -> &str {
    entry.split_once('=').map_or(entry, |(key, _)| key)
}

/// Splits editor rows into active `Exec` assignments and disabled entries.
///
/// Empty and duplicated keys are skipped, matching the validation performed by
/// [`EnvEditor::refresh`]. Active values are quoted for the desktop entry;
/// disabled values stay raw because they are stored in the configuration.
fn partition_environment(rows: &[(String, String, EnvState)]) -> (Vec<String>, Vec<String>) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, _, _) in rows {
        if !key.is_empty() {
            *counts.entry(key.as_str()).or_default() += 1;
        }
    }

    let mut active = Vec::new();
    let mut disabled = Vec::new();
    for (key, value, state) in rows {
        if key.is_empty() || counts.get(key.as_str()).copied().unwrap_or(0) != 1 {
            continue;
        }

        match state {
            EnvState::Enabled => {
                active.push(format!("{key}={}", gearlever_core::args::quote(value)));
            }
            EnvState::Disabled => {
                disabled.push(format!("{key}={value}"));
            }
        }
    }

    (active, disabled)
}

/// Returns whether an environment entry has the `KEY=value` shape.
fn is_history_variable(entry: &str) -> bool {
    entry
        .split_once('=')
        .is_some_and(|(key, _)| !key.is_empty())
}

/// Builds the popover that lists environment variables used by any application.
fn environment_history_popover(editor: &EnvEditor, history: &[String]) -> gtk::Popover {
    let popover = gtk::Popover::new();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);

    let title = gtk::Label::new(Some(&t("Environment variable history")));
    title.add_css_class("heading");
    title.set_halign(gtk::Align::Start);
    content.append(&title);

    let empty = gtk::Label::new(Some(&t("No environment variables yet")));
    empty.add_css_class("dim-label");
    empty.set_margin_top(12);
    empty.set_margin_bottom(12);
    empty.set_visible(history.is_empty());
    content.append(&empty);

    if !history.is_empty() {
        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scrolled.set_min_content_width(380);
        scrolled.set_max_content_height(320);
        scrolled.set_propagate_natural_height(true);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        let entries = Rc::new(RefCell::new(history.to_vec()));
        let remaining = Rc::new(Cell::new(history.len()));

        for entry in history {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(true);
            row.set_tooltip_text(Some(entry.as_str()));

            let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            line.set_margin_top(4);
            line.set_margin_bottom(4);
            line.set_margin_start(12);
            line.set_margin_end(6);

            let label = gtk::Label::new(Some(entry.as_str()));
            label.set_halign(gtk::Align::Start);
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            line.append(&label);

            let delete = gtk::Button::from_icon_name("user-trash-symbolic");
            delete.add_css_class("destructive-action");
            delete.set_valign(gtk::Align::Center);
            delete.set_tooltip_text(Some(&t("Remove from history")));

            let sender = editor.sender.clone();
            let entry_for_delete = entry.clone();
            let list_for_delete = list.clone();
            let row_for_delete = row.clone();
            let entries_for_delete = Rc::clone(&entries);
            let remaining_for_delete = Rc::clone(&remaining);
            let scrolled_for_delete = scrolled.clone();
            let empty_for_delete = empty.clone();
            delete.connect_clicked(move |_| {
                sender.input(Msg::ForgetEnvironment(entry_for_delete.clone()));
                entries_for_delete
                    .borrow_mut()
                    .retain(|candidate| candidate != &entry_for_delete);
                list_for_delete.remove(&row_for_delete);

                let remaining = remaining_for_delete.get().saturating_sub(1);
                remaining_for_delete.set(remaining);
                if remaining == 0 {
                    scrolled_for_delete.set_visible(false);
                    empty_for_delete.set_visible(true);
                }
            });

            line.append(&delete);
            row.set_child(Some(&line));
            list.append(&row);
        }

        let editor = editor.clone();
        let popover_for_activation = popover.clone();
        let entries_for_activation = Rc::clone(&entries);
        list.connect_row_activated(move |_, row| {
            let Ok(index) = usize::try_from(row.index()) else {
                return;
            };
            let entry = entries_for_activation.borrow().get(index).cloned();
            let Some(entry) = entry else {
                return;
            };
            let (key, value) = entry.split_once('=').unwrap_or((entry.as_str(), ""));
            popover_for_activation.popdown();
            editor.apply_history(key, value);
        });

        scrolled.set_child(Some(&list));
        content.append(&scrolled);
    }

    popover.set_child(Some(&content));
    popover
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

/// Opacity of the text entries of a disabled environment variable.
const DISABLED_ENV_OPACITY: f64 = 0.55;

/// Icon size used by the rows of the multi-update page.
const MULTI_ROW_ICON: i32 = 45;

/// Width of the small preview window.
const PREVIEW_WINDOW_WIDTH: i32 = 600;
/// Extra height that keeps the preview content out of the scroll range.
///
/// GTK reserves part of the window height for the decoration and the hidden
/// rows, so asking only for the measured content leaves a small scrollbar.
const PREVIEW_WINDOW_EXTRA_HEIGHT: i32 = 24;
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

/// Returns whether an installed app refers to the same application as `metadata`.
fn matches_metadata(app: &InstalledApp, metadata: &AppMetadata) -> bool {
    app.matches_name(&metadata.name)
}

/// Returns the installed app that conflicts with `metadata`, if any.
///
/// Gear LeverR's own entry is matched by its marker so a new version can
/// replace it even when the display name was translated or edited.
fn conflict_app<'a>(apps: &'a [InstalledApp], metadata: &AppMetadata) -> Option<&'a InstalledApp> {
    if self_install::is_self_metadata(metadata)
        && let Some(app) = apps.iter().find(|app| app.self_entry)
    {
        return Some(app);
    }

    apps.iter().find(|app| matches_metadata(app, metadata))
}

/// Finds an installed app that matches `metadata` and was not replaced yet.
fn free_conflict_app<'a>(
    apps: &'a [InstalledApp],
    metadata: &AppMetadata,
    used: &HashSet<PathBuf>,
) -> Option<&'a InstalledApp> {
    if self_install::is_self_metadata(metadata)
        && let Some(app) = apps
            .iter()
            .find(|app| app.self_entry && !used.contains(&app.image_path))
    {
        return Some(app);
    }

    apps.iter()
        .find(|app| !used.contains(&app.image_path) && matches_metadata(app, metadata))
}

/// Builds the data [`install::integrate`] needs to replace an installed app.
fn previous_install(app: &InstalledApp) -> PreviousInstall {
    PreviousInstall {
        image_path: app.image_path.clone(),
        desktop_path: app.desktop_path.clone(),
        exec_arguments: app.exec_arguments.clone(),
        env_variables: app.env_variables.clone(),
    }
}

/// Classifies a loaded AppImage that cannot be integrated in a batch.
fn ignored_reason(file: &LoadedFile, installed: Option<PathBuf>) -> Option<IgnoredReason> {
    if let Some(path) = installed {
        Some(IgnoredReason::AlreadyInstalled(path))
    } else if !file.candidate.arch.is_compatible_with_host() {
        Some(IgnoredReason::ForeignArchitecture)
    } else {
        None
    }
}

/// Resolves the batch into the integrations that still have to run.
///
/// Skipped AppImages are left out, and a `Replace` without a matching
/// installed app falls back to `KeepBoth` inside [`install::integrate`].
fn install_jobs(
    pending: &[CandidateFile],
    apps: &[InstalledApp],
    decisions: &HashMap<PathBuf, Conflict>,
    skipped: &HashSet<PathBuf>,
) -> Vec<InstallJob> {
    let mut used = HashSet::new();

    pending
        .iter()
        .filter(|candidate| !skipped.contains(&candidate.path))
        .map(|candidate| {
            let conflict = decisions
                .get(&candidate.path)
                .copied()
                .unwrap_or(Conflict::KeepBoth);
            let previous = if conflict == Conflict::Replace {
                let app = free_conflict_app(apps, &candidate.metadata, &used);
                app.map(|app| {
                    used.insert(app.image_path.clone());
                    previous_install(app)
                })
            } else {
                None
            };

            InstallJob {
                path: candidate.path.clone(),
                metadata: candidate.metadata.clone(),
                conflict,
                previous,
            }
        })
        .collect()
}

/// Keeps only the AppImages that failed or were skipped in the batch.
fn retain_pending(
    pending: &mut Vec<CandidateFile>,
    failed: &[PathBuf],
    skipped: &HashSet<PathBuf>,
) {
    let keep: HashSet<&Path> = failed
        .iter()
        .map(PathBuf::as_path)
        .chain(skipped.iter().map(PathBuf::as_path))
        .collect();
    pending.retain(|candidate| keep.contains(candidate.path.as_path()));
}

/// Returns whether launching `app` would start Gear LeverR itself.
fn is_self_launch(app: &InstalledApp) -> bool {
    app.self_entry || running_appimage().is_some_and(|image| same_file(&image, &app.image_path))
}

fn save_github_token(
    row: &adw::PasswordEntryRow,
    status: &adw::ActionRow,
    store: &TokenStore,
) -> bool {
    let token = row.text().trim().to_owned();

    if token.is_empty() {
        status.set_subtitle(&t("Enter a token first"));
        return false;
    }
    if !valid_token(&token) {
        status.set_subtitle(&t("The token contains invalid characters"));
        return false;
    }

    match store.set(&token) {
        Ok(()) => {
            row.set_title(&t("Personal access token"));
            status.set_subtitle(&t("GitHub token saved. Use Test to verify it."));
            true
        }
        Err(error) => {
            let error = error.to_string();
            status.set_subtitle(&tf(
                "Could not save the token: {error}",
                &[("error", &error)],
            ));
            false
        }
    }
}

fn clear_github_token(row: &adw::PasswordEntryRow, status: &adw::ActionRow, store: &TokenStore) {
    match store.clear() {
        Ok(()) => {
            row.set_text("");
            row.set_title(&t("Personal access token"));
            status.set_subtitle(&t("GitHub token cleared"));
        }
        Err(error) => {
            let error = error.to_string();
            status.set_subtitle(&tf(
                "Could not clear the token: {error}",
                &[("error", &error)],
            ));
        }
    }
}

fn initial_github_status(store: &TokenStore, status: TokenStatus, lookup: &TokenLookup) -> String {
    match lookup {
        TokenLookup::Locked => {
            return t(
                "Token configured, but the keyring is locked. Unlock it or enter a new token.",
            );
        }
        TokenLookup::Unavailable | TokenLookup::Missing if status == TokenStatus::None => {
            return t("Verify your token works and check rate limit status");
        }
        TokenLookup::Unavailable | TokenLookup::Missing => {
            return t(
                "Token configured, but it could not be loaded. Unlock the system keyring or enter a new token.",
            );
        }
        TokenLookup::Found(_) => {}
    }

    match status {
        TokenStatus::Valid => match store.expires_at() {
            Some(expires_at) => tf(
                "Token configured · valid · expires {expires_at}",
                &[("expires_at", &expires_at)],
            ),
            None => t("Token configured · valid"),
        },
        TokenStatus::Invalid => match store.expires_at() {
            Some(expires_at) => tf(
                "Token configured · invalid or expired · expires {expires_at}",
                &[("expires_at", &expires_at)],
            ),
            None => t("Token configured · invalid or expired"),
        },
        _ => t("Token configured. Use Test to verify it."),
    }
}

fn start_token_test(
    token: String,
    status: &adw::ActionRow,
    button: &gtk::Button,
    spinner: &gtk::Spinner,
    sender: &ComponentSender<AppModel>,
) {
    button.set_sensitive(false);
    spinner.set_visible(true);
    status.set_subtitle(&t("Testing..."));

    let row_ref = glib::SendWeakRef::from(status.downgrade());
    let button_ref = glib::SendWeakRef::from(button.downgrade());
    let spinner_ref = glib::SendWeakRef::from(spinner.downgrade());
    let sender = sender.clone();

    std::thread::spawn(move || {
        let result = update::github::test_token(&token);
        sender.input(Msg::GithubTokenTested(result.clone()));
        glib::MainContext::default().invoke(move || {
            if let Some(row) = row_ref.upgrade() {
                row.set_subtitle(&token_check_message(&result));
            }
            if let Some(button) = button_ref.upgrade() {
                button.set_sensitive(true);
            }
            if let Some(spinner) = spinner_ref.upgrade() {
                spinner.set_visible(false);
            }
        });
    });
}

fn token_check_message(check: &TokenCheck) -> String {
    match check {
        TokenCheck::Valid {
            remaining,
            limit,
            expires_at,
        } => match (remaining, limit, expires_at) {
            (Some(remaining), Some(limit), Some(expires_at)) => {
                let remaining = remaining.to_string();
                let limit = limit.to_string();
                tf(
                    "Token is valid — {remaining} / {limit} requests remaining · expires {expires_at}",
                    &[
                        ("remaining", &remaining),
                        ("limit", &limit),
                        ("expires_at", expires_at),
                    ],
                )
            }
            (Some(remaining), Some(limit), None) => {
                let remaining = remaining.to_string();
                let limit = limit.to_string();
                tf(
                    "Token is valid — {remaining} / {limit} requests remaining",
                    &[("remaining", &remaining), ("limit", &limit)],
                )
            }
            (_, _, Some(expires_at)) => tf(
                "Token is valid — expires {expires_at}",
                &[("expires_at", expires_at)],
            ),
            _ => t("Token is valid"),
        },
        TokenCheck::Invalid => t("Authentication failed — token is invalid or expired"),
        TokenCheck::Forbidden => t("Access forbidden — token lacks required permissions"),
        TokenCheck::RateLimited => t("Rate limit exceeded — try again later"),
        TokenCheck::Unexpected(status) => {
            let status = status.to_string();
            tf("Unexpected response ({status})", &[("status", &status)])
        }
        TokenCheck::Network(error) => tf("Connection error: {error}", &[("error", error)]),
        _ => t("Unexpected response"),
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "the update progress does not need exact precision"
)]
fn progress_fraction(done: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    }
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

/// Clamps a saved window size to the current monitor.
fn fit_window_size(width: i32, height: i32) -> (i32, i32) {
    let width = width.clamp(480, 10_000);
    let height = height.clamp(360, 10_000);
    let Some(display) = gdk::Display::default() else {
        return (width, height);
    };
    let monitors = display.monitors();
    let Some(monitor) = (0..monitors.n_items())
        .find_map(|index| monitors.item(index).and_downcast::<gdk::Monitor>())
    else {
        return (width, height);
    };
    let geometry = monitor.geometry();

    (
        width.min(geometry.width().saturating_sub(80)).max(480),
        height.min(geometry.height().saturating_sub(80)).max(360),
    )
}

fn section_header(title: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(title));
    label.add_css_class("title-4");
    label.set_halign(gtk::Align::Start);
    label.set_margin_start(6);
    label
}

fn clear_listbox(listbox: &gtk::ListBox) {
    while let Some(child) = listbox.first_child() {
        listbox.remove(&child);
    }
}

/// Builds the small icon and label that describes a batch row.
fn status_suffix(icon: &str, text: &str, tooltip: Option<&str>, css: &str) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    container.set_valign(gtk::Align::Center);

    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    image.add_css_class(css);
    container.append(&image);

    let label = gtk::Label::new(Some(text));
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    container.append(&label);

    if let Some(tooltip) = tooltip {
        container.set_tooltip_text(Some(tooltip));
    }

    container.upcast()
}

/// Builds the inline warning shown when an AppImage targets another architecture.
///
/// `AdwBanner` is libadwaita 1.3 and the project targets 1.2, so the banner is
/// assembled from a styled box and a label.
fn architecture_banner() -> gtk::Widget {
    let banner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    banner.add_css_class("arch-warning");
    banner.set_margin_top(12);
    banner.set_margin_start(12);
    banner.set_margin_end(12);
    banner.set_valign(gtk::Align::Center);

    let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    icon.set_pixel_size(24);
    icon.add_css_class("warning");
    banner.append(&icon);

    let label = gtk::Label::new(Some(&t(
        "This app might not be compatible with your system architecture",
    )));
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    label.set_wrap(true);
    banner.append(&label);

    banner.upcast()
}

/// Builds the update pills shown on the installed application details page.
fn update_source_pills(source: &Result<Option<update::SourceInfo>, CoreError>) -> Vec<String> {
    match source {
        Ok(Some(info)) if !info.valid => vec![tf(
            "Updates: {source} (incomplete)",
            &[("source", info.kind.label())],
        )],
        Ok(Some(info)) => {
            let mut pills = vec![tf("Updates: {source}", &[("source", info.kind.label())])];
            if info.zsync {
                pills.push("zsync".to_owned());
            }
            pills
        }
        // An application without an update source simply has no pill.
        Ok(None) => Vec::new(),
        Err(error) => {
            tracing::debug!(%error, "cannot describe the update source");
            vec![t("Invalid update source")]
        }
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

/// Builds a short, readable message for an AppImage that failed to load.
///
/// The toast is narrow, so the full path is left out and only the file name is
/// shown. Errors that carry their own path are reduced to their cause; the
/// caller logs the detailed error with the full path.
#[must_use]
fn load_error_message(path: &Path, error: &CoreError) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );

    let detail = match error {
        CoreError::MissingExtractor { kind, .. } => match kind {
            PayloadKind::Iso9660 => {
                t("This AppImage uses an ISO9660 payload. Install 7-Zip (7zz or 7z) to read it.")
            }
            PayloadKind::Dwarfs => t(
                "The bundled DwarFS extractor is missing. Reinstall Gear LeverR or install dwarfsextract.",
            ),
            _ => t(
                "No extractor is available to read this AppImage. Install dwarfsextract or 7-Zip.",
            ),
        },
        CoreError::NotAnAppImage { .. } => t("Not an AppImage"),
        CoreError::MissingDesktopEntry(_) => t("The AppImage does not contain a .desktop file."),
        CoreError::DesktopEntryNotUtf8(_) => t("The desktop entry is not valid UTF-8."),
        CoreError::Io(_) => t("The file could not be read."),
        CoreError::InvalidElf(_) => t("The AppImage runtime is invalid."),
        CoreError::Squashfs(_) => t("The AppImage filesystem could not be read."),
        _ => t("The AppImage could not be read."),
    };

    format!("{detail} — {name}")
}

/// Returns whether a load failure needs the ISO9660 install dialog.
#[must_use]
fn load_failure_kind(error: &CoreError) -> LoadFailureKind {
    if matches!(
        error,
        CoreError::MissingExtractor {
            kind: PayloadKind::Iso9660,
            ..
        }
    ) {
        LoadFailureKind::Iso9660Extractor
    } else {
        LoadFailureKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(name: &str) -> AppMetadata {
        AppMetadata {
            name: name.to_owned(),
            comment: String::new(),
            exec: String::new(),
            try_exec: String::new(),
            icon_name: None,
            terminal: false,
            version: None,
            desktop_file: Some("test.desktop".to_owned()),
            desktop_contents: String::new(),
            icon: None,
        }
    }

    fn installed(name: &str, image: &str) -> InstalledApp {
        InstalledApp {
            name: name.to_owned(),
            appimage_name: Some(name.to_owned()),
            description: String::new(),
            version: None,
            image_path: PathBuf::from(image),
            desktop_path: PathBuf::from(format!("{image}.desktop")),
            desktop_id: String::new(),
            icon: None,
            icon_path: None,
            exec_arguments: "--custom".to_owned(),
            env_variables: vec!["CUSTOM=1".to_owned()],
            terminal: false,
            size: 0,
            external: false,
            self_entry: false,
        }
    }

    fn self_installed(name: &str, image: &str) -> InstalledApp {
        let mut app = installed(name, image);
        app.self_entry = true;
        app
    }

    fn candidate(name: &str, image: &str) -> CandidateFile {
        CandidateFile {
            path: PathBuf::from(image),
            metadata: metadata(name),
            arch: ElfArch::X86_64,
        }
    }

    fn loaded(name: &str, image: &str, arch: ElfArch) -> LoadedFile {
        LoadedFile {
            candidate: CandidateFile {
                path: PathBuf::from(image),
                metadata: metadata(name),
                arch,
            },
            installed: None,
        }
    }

    #[test]
    fn hides_the_update_pill_without_a_source() {
        assert!(update_source_pills(&Ok(None)).is_empty());
    }

    #[test]
    fn describes_a_valid_update_source_and_zsync() {
        let config = BTreeMap::from([
            ("manager".to_owned(), "GithubUpdater".to_owned()),
            ("repo".to_owned(), "owner/repo".to_owned()),
            (
                "repo_filename".to_owned(),
                "App-*.AppImage.zsync".to_owned(),
            ),
        ]);
        let source = update::source_info(Path::new("/missing"), &config)
            .expect("configured source")
            .expect("source present");

        assert_eq!(
            update_source_pills(&Ok(Some(source))),
            vec![
                tf(
                    "Updates: {source}",
                    &[("source", ManagerKind::Github.label())]
                ),
                "zsync".to_owned(),
            ]
        );
    }

    #[test]
    fn flags_incomplete_and_invalid_update_sources() {
        let config = BTreeMap::from([("manager".to_owned(), "GitlabUpdater".to_owned())]);
        let incomplete = update::source_info(Path::new("/missing"), &config)
            .expect("configured source")
            .expect("source present");
        assert_eq!(
            update_source_pills(&Ok(Some(incomplete))),
            vec![tf(
                "Updates: {source} (incomplete)",
                &[("source", ManagerKind::Gitlab.label())],
            )]
        );

        let error = CoreError::InvalidElf("bad header");
        assert_eq!(
            update_source_pills(&Err(error)),
            vec![t("Invalid update source")]
        );
    }

    #[test]
    fn puts_the_load_error_before_the_file_name() {
        let path = Path::new("/home/tester/Descargas/Programs/Ghostty-1.4.0-x86_64.AppImage");
        let error = CoreError::MissingExtractor {
            path: path.to_path_buf(),
            kind: PayloadKind::Dwarfs,
        };

        let message = load_error_message(path, &error);

        assert!(
            message.starts_with(&t(
                "The bundled DwarFS extractor is missing. Reinstall Gear LeverR or install dwarfsextract."
            )),
            "{message}"
        );
        assert!(
            message.ends_with("— Ghostty-1.4.0-x86_64.AppImage"),
            "{message}"
        );
        assert!(!message.contains("/home/tester/Descargas"), "{message}");
    }

    #[test]
    fn explains_iso9660_before_the_file_name() {
        let path = Path::new("/home/tester/Descargas/Programs/Legacy.AppImage");
        let error = CoreError::MissingExtractor {
            path: path.to_path_buf(),
            kind: PayloadKind::Iso9660,
        };

        let message = load_error_message(path, &error);

        assert!(
            message.starts_with(&t(
                "This AppImage uses an ISO9660 payload. Install 7-Zip (7zz or 7z) to read it."
            )),
            "{message}"
        );
        assert!(message.ends_with("— Legacy.AppImage"), "{message}");
        assert!(!message.contains("/home/tester/Descargas"), "{message}");
        assert_eq!(load_failure_kind(&error), LoadFailureKind::Iso9660Extractor);
    }

    #[test]
    fn generic_load_errors_do_not_leak_the_path() {
        let path = Path::new("/home/tester/Descargas/Programs/Broken.AppImage");
        let error = CoreError::InvalidName(path.to_path_buf());

        let message = load_error_message(path, &error);

        assert_eq!(
            message,
            format!("{} — Broken.AppImage", t("The AppImage could not be read."))
        );
        assert!(!message.contains("/home/tester/Descargas"), "{message}");
        assert_eq!(load_failure_kind(&error), LoadFailureKind::Other);
    }

    #[test]
    fn explains_io_errors_without_the_path() {
        let path = Path::new("/home/tester/Descargas/Programs/Broken.AppImage");
        let error = CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));

        let message = load_error_message(path, &error);

        assert_eq!(
            message,
            format!("{} — Broken.AppImage", t("The file could not be read."))
        );
        assert!(!message.contains("/home/tester/Descargas"), "{message}");
    }

    #[test]
    fn falls_back_to_the_path_when_it_has_no_file_name() {
        let path = Path::new("/");
        let error = CoreError::NotAnAppImage {
            path: path.to_path_buf(),
            magic: [0, 0, 0],
        };

        let message = load_error_message(path, &error);

        assert_eq!(message, format!("{} — /", t("Not an AppImage")));
    }

    #[test]
    fn detects_conflicts_by_name() {
        let apps = vec![installed("Test App", "/managed/test.appimage")];

        assert!(conflict_app(&apps, &metadata("Test App")).is_some());
        assert!(conflict_app(&apps, &metadata("Other App")).is_none());
        assert!(conflict_app(&[], &metadata("Test App")).is_none());
    }

    #[test]
    fn detects_conflicts_through_the_appimage_name() {
        let mut app = installed("Multi Test (abc123)", "/managed/test.appimage");
        app.appimage_name = Some("Multi Test".to_owned());

        assert!(conflict_app(&[app], &metadata("Multi Test")).is_some());
    }

    #[test]
    fn prefers_the_self_entry_when_replacing_gearlever() {
        let apps = vec![
            installed("Gear LeverR", "/managed/other.appimage"),
            self_installed("Gear LeverR", "/managed/self.appimage"),
        ];
        let mut candidate = metadata("Gear LeverR");
        candidate.desktop_file = Some("io.github.alarez777.GearLeverR.desktop".to_owned());

        let found = conflict_app(&apps, &candidate).unwrap();

        assert!(found.self_entry);
        assert_eq!(found.image_path, PathBuf::from("/managed/self.appimage"));
    }

    #[test]
    fn builds_jobs_with_the_replace_previous_install() {
        let apps = vec![installed("Test App", "/managed/old.appimage")];
        let pending = vec![
            candidate("Test App", "/tmp/new.appimage"),
            candidate("Other App", "/tmp/other.appimage"),
        ];
        let decisions = HashMap::from([(PathBuf::from("/tmp/new.appimage"), Conflict::Replace)]);

        let jobs = install_jobs(&pending, &apps, &decisions, &HashSet::new());

        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].conflict, Conflict::Replace);
        let previous = jobs[0].previous.as_ref().unwrap();
        assert_eq!(previous.image_path, PathBuf::from("/managed/old.appimage"));
        assert_eq!(previous.exec_arguments, "--custom");
        assert_eq!(previous.env_variables, vec!["CUSTOM=1"]);
        assert_eq!(jobs[1].conflict, Conflict::KeepBoth);
        assert!(jobs[1].previous.is_none());
    }

    #[test]
    fn replaces_each_installed_version_at_most_once() {
        let apps = vec![
            installed("Test App", "/managed/old-1.appimage"),
            installed("Test App", "/managed/old-2.appimage"),
        ];
        let pending = vec![
            candidate("Test App", "/tmp/new-1.appimage"),
            candidate("Test App", "/tmp/new-2.appimage"),
        ];
        let decisions = HashMap::from([
            (PathBuf::from("/tmp/new-1.appimage"), Conflict::Replace),
            (PathBuf::from("/tmp/new-2.appimage"), Conflict::Replace),
        ]);

        let jobs = install_jobs(&pending, &apps, &decisions, &HashSet::new());

        assert_eq!(
            jobs[0].previous.as_ref().unwrap().image_path,
            PathBuf::from("/managed/old-1.appimage")
        );
        assert_eq!(
            jobs[1].previous.as_ref().unwrap().image_path,
            PathBuf::from("/managed/old-2.appimage")
        );
    }

    #[test]
    fn falls_back_to_keep_both_when_no_previous_is_left() {
        let apps = vec![installed("Test App", "/managed/old.appimage")];
        let pending = vec![
            candidate("Test App", "/tmp/new-1.appimage"),
            candidate("Test App", "/tmp/new-2.appimage"),
        ];
        let decisions = HashMap::from([
            (PathBuf::from("/tmp/new-1.appimage"), Conflict::Replace),
            (PathBuf::from("/tmp/new-2.appimage"), Conflict::Replace),
        ]);

        let jobs = install_jobs(&pending, &apps, &decisions, &HashSet::new());

        assert!(jobs[0].previous.is_some());
        assert!(jobs[1].previous.is_none());
    }

    #[test]
    fn leaves_skipped_appimages_out_of_the_batch() {
        let pending = vec![
            candidate("Test App", "/tmp/new.appimage"),
            candidate("Other App", "/tmp/other.appimage"),
        ];
        let skipped = HashSet::from([PathBuf::from("/tmp/new.appimage")]);

        let jobs = install_jobs(&pending, &[], &HashMap::new(), &skipped);

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].path, PathBuf::from("/tmp/other.appimage"));
        assert_eq!(jobs[0].conflict, Conflict::KeepBoth);
    }

    #[test]
    fn keeps_failed_and_skipped_appimages_for_a_retry() {
        let mut pending = vec![
            candidate("Integrated", "/tmp/integrated.appimage"),
            candidate("Failed", "/tmp/failed.appimage"),
            candidate("Skipped", "/tmp/skipped.appimage"),
        ];
        let failed = vec![PathBuf::from("/tmp/failed.appimage")];
        let skipped = HashSet::from([PathBuf::from("/tmp/skipped.appimage")]);

        retain_pending(&mut pending, &failed, &skipped);

        assert_eq!(pending.len(), 2);
        assert!(
            pending
                .iter()
                .any(|candidate| candidate.path == Path::new("/tmp/failed.appimage"))
        );
        assert!(
            pending
                .iter()
                .any(|candidate| candidate.path == Path::new("/tmp/skipped.appimage"))
        );
        assert!(
            !pending
                .iter()
                .any(|candidate| candidate.path == Path::new("/tmp/integrated.appimage"))
        );
    }

    #[test]
    fn classifies_batch_candidates_that_cannot_be_integrated() {
        let host = ElfArch::host().unwrap_or(ElfArch::X86_64);
        let foreign = if host == ElfArch::X86_64 {
            ElfArch::Aarch64
        } else {
            ElfArch::X86_64
        };

        let compatible = loaded("Compatible", "/tmp/compatible.appimage", host);
        assert!(ignored_reason(&compatible, None).is_none());

        let incompatible = loaded("Foreign", "/tmp/foreign.appimage", foreign);
        assert_eq!(
            ignored_reason(&incompatible, None),
            Some(IgnoredReason::ForeignArchitecture)
        );

        let installed_path = PathBuf::from("/managed/compatible.appimage");
        assert_eq!(
            ignored_reason(&compatible, Some(installed_path.clone())),
            Some(IgnoredReason::AlreadyInstalled(installed_path))
        );
    }

    #[test]
    fn preview_windows_hide_the_back_button() {
        assert!(!Page::Details.shows_left_button(true));
        assert!(!Page::MultiInstall.shows_left_button(true));
        assert!(!Page::MultiUpdate.shows_left_button(true));

        assert!(Page::List.shows_left_button(true));
        assert!(Page::List.shows_left_button(false));
        assert!(Page::Details.shows_left_button(false));
        assert!(Page::MultiInstall.shows_left_button(false));
    }

    #[test]
    fn partitions_environment_into_active_and_disabled_entries() {
        let rows = vec![
            ("A".to_owned(), "1".to_owned(), EnvState::Enabled),
            ("B".to_owned(), "two words".to_owned(), EnvState::Disabled),
            ("C".to_owned(), "3".to_owned(), EnvState::Enabled),
            ("C".to_owned(), "4".to_owned(), EnvState::Disabled),
            (String::new(), String::new(), EnvState::Enabled),
        ];

        let (active, disabled) = partition_environment(&rows);

        assert_eq!(active, vec!["A=1"]);
        assert_eq!(disabled, vec!["B=two words"]);
    }

    #[test]
    fn accepts_valid_environment_entries_for_history() {
        assert!(is_history_variable("GTK_THEME=Adwaita:dark"));
        assert!(is_history_variable("EMPTY="));
        assert!(is_history_variable("DESKTOPINTEGRATION=1"));
        assert!(!is_history_variable("=missing-key"));
        assert!(!is_history_variable(""));
    }
}
