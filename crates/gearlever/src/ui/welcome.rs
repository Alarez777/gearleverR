//! First-run window that integrates Gear LeverR into the desktop.
//!
//! The window is shown while Gear LeverR is running from an AppImage that has no
//! usable desktop entry of its own. It offers the settings that matter before the
//! first integration and writes the entry with [`gearlever_core::self_install`].

#![expect(unreachable_pub, reason = "private module of the binary")]

use std::cell::Cell;
use std::rc::Rc;

use gearlever_core::self_install;
use gearlever_core::system::{FuseComponent, FuseProblem, FuseReport};
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::t;
use crate::settings::Settings;

use super::{AppModel, FUSE_DOCS_URL, Msg};

/// Maximum width of the first-run content inside the window.
const WELCOME_MAX_CONTENT_WIDTH: i32 = 720;
/// Maximum width of the first-run window.
const WELCOME_MAX_WINDOW_WIDTH: i32 = 1000;
/// Maximum height of the first-run window.
const WELCOME_MAX_HEIGHT: i32 = 860;
/// Minimum width of the first-run window; it must fit the content plus margins.
const WELCOME_MIN_WIDTH: i32 = 720;
/// Minimum height of the first-run window.
const WELCOME_MIN_HEIGHT: i32 = 680;
/// Horizontal margin of the first-run window body.
const WELCOME_BODY_MARGIN: i32 = 18;

/// Widgets of the first-run window that the model keeps updating.
pub(crate) struct Welcome {
    pub window: adw::Window,
    pub toast: adw::ToastOverlay,
    pub folder_row: adw::ActionRow,
    pub install_button: gtk::Button,
    pub skip_button: gtk::Button,
    pub close_button: gtk::Button,
    pub spinner: gtk::Spinner,
    fuse_section: gtk::Box,
    fuse: Cell<FuseReport>,
    fetch_switch: gtk::Switch,
    fetch_guard: Rc<Cell<bool>>,
}

impl Welcome {
    /// Disables the controls and shows the installation spinner.
    pub fn begin_install(&self) {
        self.disable_controls();
        self.spinner.set_visible(true);
        self.spinner.set_spinning(true);
        self.install_button.set_label(&t("Installing..."));
    }

    /// Restores the controls after a failed installation.
    pub fn end_install(&self) {
        self.enable_controls();
        self.spinner.set_visible(false);
        self.spinner.set_spinning(false);
        self.install_button
            .set_label(&t("Install and open Gear LeverR"));
    }

    fn enable_controls(&self) {
        self.update_install_button();
        self.skip_button.set_sensitive(true);
        self.close_button.set_sensitive(true);
        self.fuse_section.set_sensitive(true);
    }

    fn disable_controls(&self) {
        self.install_button.set_sensitive(false);
        self.skip_button.set_sensitive(false);
        self.close_button.set_sensitive(false);
        self.fuse_section.set_sensitive(false);
    }

    /// Updates the install button from the latest FUSE report.
    fn update_install_button(&self) {
        let ready = self.fuse.get().all_ready();
        self.install_button.set_sensitive(ready);
        let tooltip = (!ready).then(|| t("Resolve the FUSE issues first"));
        self.install_button.set_tooltip_text(tooltip.as_deref());
    }

    /// Rechecks FUSE, rebuilds the notice and updates the install button.
    pub fn recheck_dependencies(&self, sender: &relm4::ComponentSender<AppModel>) {
        if !self.fuse_section.is_sensitive() {
            return;
        }

        let report = FuseReport::detect();
        self.fuse.set(report);

        while let Some(child) = self.fuse_section.first_child() {
            self.fuse_section.remove(&child);
        }
        if let Some(notice) = fuse_notice(report) {
            self.fuse_section.append(&notice);
        }
        self.fuse_section.append(&fuse_dependencies(report, sender));

        self.update_install_button();
        if report.all_ready() {
            self.toast(&t("Dependencies are ready"));
        } else {
            self.toast(&t("Some dependencies are still missing"));
        }
    }

    /// Puts the login switch back when the autostart entry cannot be written.
    pub fn revert_fetch_updates(&self) {
        let active = !self.fetch_switch.is_active();
        self.fetch_guard.set(true);
        self.fetch_switch.set_active(active);
        self.fetch_guard.set(false);
    }

    /// Shows a message inside the window.
    pub fn toast(&self, message: &str) {
        self.toast.add_toast(adw::Toast::new(message));
    }
}

/// Builds and presents the first-run window.
#[expect(
    clippy::too_many_lines,
    reason = "the first-run window is assembled once, in one place"
)]
pub(crate) fn present(
    parent: &adw::ApplicationWindow,
    settings: &Settings,
    sender: &relm4::ComponentSender<AppModel>,
) -> Welcome {
    let parent_width = parent.width().max(settings.values().window_width);
    let parent_height = parent.height().max(settings.values().window_height);
    let width = parent_width.clamp(WELCOME_MIN_WIDTH, WELCOME_MAX_WINDOW_WIDTH);
    let height = parent_height.clamp(WELCOME_MIN_HEIGHT, WELCOME_MAX_HEIGHT);

    let window = adw::Window::builder()
        .title(t("Welcome to Gear LeverR"))
        .transient_for(parent)
        .modal(true)
        .default_width(width)
        .default_height(height)
        .build();

    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);

    let close_button = gtk::Button::from_icon_name("window-close-symbolic");
    close_button.set_tooltip_text(Some(&t("Close")));
    let sender_close = sender.clone();
    close_button.connect_clicked(move |_| sender_close.input(Msg::WelcomeSkip));
    header.pack_start(&close_button);

    let icon = gtk::Image::from_icon_name(application_icon(&window));
    icon.set_pixel_size(96);
    icon.set_halign(gtk::Align::Center);

    let intro_title = gtk::Label::new(Some(&t("Welcome to Gear LeverR")));
    intro_title.add_css_class("title-1");
    intro_title.set_justify(gtk::Justification::Center);
    intro_title.set_wrap(true);

    let intro_text = gtk::Label::new(Some(&t(
        "Gear LeverR is not integrated into your desktop yet. Choose how it should be installed and where to keep your AppImages.",
    )));
    intro_text.add_css_class("dim-label");
    intro_text.set_justify(gtk::Justification::Center);
    intro_text.set_wrap(true);
    intro_text.set_max_width_chars(60);

    let intro = gtk::Box::new(gtk::Orientation::Vertical, 8);
    intro.set_halign(gtk::Align::Center);
    intro.set_margin_top(12);
    intro.append(&icon);
    intro.append(&intro_title);
    intro.append(&intro_text);

    let location = adw::PreferencesGroup::new();
    location.set_title(&t("Set the AppImage location"));

    let folder_row = adw::ActionRow::builder()
        .title(t("AppImage default location"))
        .subtitle(
            settings
                .values()
                .appimages_dir
                .to_string_lossy()
                .into_owned(),
        )
        .build();
    folder_row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));

    let choose = gtk::Button::from_icon_name("folder-open-symbolic");
    choose.set_valign(gtk::Align::Center);
    choose.set_tooltip_text(Some(&t("Select a folder")));
    let sender_folder = sender.clone();
    let window_folder = window.clone();
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
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Accept
                && let Some(file) = dialog.file()
                && let Some(path) = file.path()
            {
                sender.input(Msg::WelcomeFolder(path));
            }
            dialog.destroy();
        });

        dialog.present();
    });
    folder_row.add_suffix(&choose);
    location.add(&folder_row);

    let files = adw::PreferencesGroup::new();
    files.set_title(&t("File management"));

    let move_row = adw::ActionRow::builder()
        .title(t("Move AppImages into the destination folder"))
        .subtitle(t(
            "Turn it off to keep the original file and clone it instead.",
        ))
        .build();
    move_row.set_activatable(false);
    let move_switch = gtk::Switch::new();
    move_switch.set_valign(gtk::Align::Center);
    move_switch.set_active(settings.values().move_on_integration);
    move_row.add_suffix(&move_switch);
    files.add(&move_row);

    let sender_move = sender.clone();
    move_switch.connect_active_notify(move |toggle| {
        sender_move.input(Msg::WelcomeMove(toggle.is_active()));
    });

    let updates = adw::PreferencesGroup::new();
    updates.set_title(&t("Updates management"));

    let fetch_row = adw::ActionRow::builder()
        .title(t("Check updates on system startup"))
        .build();
    fetch_row.set_activatable(false);
    let fetch_switch = gtk::Switch::new();
    fetch_switch.set_valign(gtk::Align::Center);
    fetch_switch.set_active(settings.values().fetch_in_background);
    fetch_row.add_suffix(&fetch_switch);
    updates.add(&fetch_row);

    let fetch_guard = Rc::new(Cell::new(false));
    let guard = Rc::clone(&fetch_guard);
    let sender_fetch = sender.clone();
    fetch_switch.connect_active_notify(move |toggle| {
        if guard.get() {
            return;
        }
        sender_fetch.input(Msg::WelcomeFetchUpdates(toggle.is_active()));
    });

    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.set_hexpand(true);
    body.set_width_request(WELCOME_MAX_CONTENT_WIDTH - 2 * WELCOME_BODY_MARGIN);
    body.set_margin_top(12);
    body.set_margin_bottom(24);
    body.set_margin_start(WELCOME_BODY_MARGIN);
    body.set_margin_end(WELCOME_BODY_MARGIN);
    body.append(&intro);
    let fuse = FuseReport::detect();
    let fuse_section = gtk::Box::new(gtk::Orientation::Vertical, 12);
    if let Some(notice) = fuse_notice(fuse) {
        fuse_section.append(&notice);
    }
    fuse_section.append(&fuse_dependencies(fuse, sender));
    body.append(&fuse_section);
    body.append(&location);
    body.append(&files);
    body.append(&updates);

    let clamp = adw::Clamp::builder()
        .maximum_size(WELCOME_MAX_CONTENT_WIDTH)
        .hexpand(true)
        .child(&body)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&clamp)
        .build();

    let install_button = gtk::Button::with_label(&t("Install and open Gear LeverR"));
    install_button.add_css_class("suggested-action");
    install_button.set_receives_default(true);
    let sender_install = sender.clone();
    install_button.connect_clicked(move |_| sender_install.input(Msg::WelcomeInstall));

    let skip_button = gtk::Button::with_label(&t("Not now"));
    let sender_skip = sender.clone();
    skip_button.connect_clicked(move |_| sender_skip.input(Msg::WelcomeSkip));

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.set_halign(gtk::Align::End);
    footer.set_margin_top(12);
    footer.set_margin_bottom(18);
    footer.set_margin_start(18);
    footer.set_margin_end(18);
    footer.append(&skip_button);
    footer.append(&spinner);
    footer.append(&install_button);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&scroll);
    content.append(&footer);

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&content));
    window.set_content(Some(&toast));

    let sender_closed = sender.clone();
    window.connect_close_request(move |_| {
        sender_closed.input(Msg::WelcomeClosed);
        gtk::glib::Propagation::Proceed
    });

    window.present();

    let welcome = Welcome {
        window,
        toast,
        folder_row,
        install_button,
        skip_button,
        close_button,
        spinner,
        fuse_section,
        fuse: Cell::new(fuse),
        fetch_switch,
        fetch_guard,
    };
    welcome.update_install_button();
    welcome
}

/// Whether a FUSE component is present, for the welcome status strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FusePresence {
    Installed,
    Missing,
}

/// Builds the FUSE notice shown when the host cannot mount AppImages.
///
/// The current AppImage runtime is statically linked with FUSE 3, so the red
/// notice is about the host pieces needed to mount, not about libfuse itself.
/// FUSE 2 is only reported as a compatibility problem for older AppImages.
#[expect(
    clippy::too_many_lines,
    reason = "the notice variants share one widget layout"
)]
fn fuse_notice(report: FuseReport) -> Option<gtk::Widget> {
    let problem = report.problem()?;

    let (class, icon_class, icon_name, title, explanation, install_lead, command) = match problem {
        FuseProblem::MissingDevice => {
            let tools_missing = !report.has_fuse2_helper() && !report.has_fuse3_helper();
            let install = if tools_missing {
                FuseComponent::All.install_command()
            } else {
                None
            };
            let (lead, command) = match (tools_missing, install) {
                (true, Some(install)) => (
                    t("Load the fuse kernel module and install FUSE with:"),
                    format!("sudo modprobe fuse\n{install}"),
                ),
                (true, None) => (
                    t(
                        "Load the fuse kernel module and install FUSE with your distribution's package manager.",
                    ),
                    "sudo modprobe fuse".to_owned(),
                ),
                (false, _) => (
                    t("Load the fuse kernel module with:"),
                    "sudo modprobe fuse".to_owned(),
                ),
            };

            (
                "fuse-error",
                "error",
                "dialog-error-symbolic",
                t("The FUSE device is not available"),
                t(
                    "Gear LeverR can still start by extracting itself, but AppImages that need FUSE will not run.",
                ),
                lead,
                Some(command),
            )
        }
        FuseProblem::MissingTools => (
            "fuse-error",
            "error",
            "dialog-error-symbolic",
            t("FUSE is not available"),
            t(
                "Gear LeverR can still start by extracting itself, but AppImages that need FUSE will not run.",
            ),
            t("Install FUSE with:"),
            FuseComponent::All.install_command(),
        ),
        FuseProblem::MissingFuse2 => (
            "fuse-warning",
            "warning",
            "dialog-warning-symbolic",
            t("FUSE 2 support is not available"),
            t(
                "AppImages built with the older runtime may not start. Gear LeverR itself uses FUSE 3 and does not need it.",
            ),
            t("Install the FUSE 2 compatibility packages with:"),
            FuseComponent::Legacy.install_command(),
        ),
        FuseProblem::MissingFuse3 => (
            "fuse-warning",
            "warning",
            "dialog-warning-symbolic",
            t("FUSE 3 support is not available"),
            t(
                "AppImages built with the newer runtime may not start. Gear LeverR can still use FUSE 2.",
            ),
            t("Install FUSE 3 with:"),
            FuseComponent::Mount.install_command(),
        ),
        _ => return None,
    };

    let notice = gtk::Box::new(gtk::Orientation::Vertical, 6);
    notice.add_css_class(class);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(24);
    icon.add_css_class(icon_class);
    let title = gtk::Label::new(Some(&title));
    title.add_css_class("heading");
    title.add_css_class(icon_class);
    title.set_halign(gtk::Align::Start);
    title.set_hexpand(true);
    title.set_wrap(true);
    header.append(&icon);
    header.append(&title);
    notice.append(&header);

    let explanation = gtk::Label::new(Some(&explanation));
    explanation.set_xalign(0.0);
    explanation.set_wrap(true);
    notice.append(&explanation);

    let install_lead = if command.is_some() {
        install_lead
    } else {
        t("Install it with your distribution's package manager.")
    };
    let lead = gtk::Label::new(Some(&install_lead));
    lead.set_xalign(0.0);
    lead.set_wrap(true);
    notice.append(&lead);

    if let Some(command) = command {
        let command = gtk::Label::new(Some(&command));
        command.add_css_class("monospace");
        command.set_selectable(true);
        command.set_wrap(true);
        command.set_xalign(0.0);
        notice.append(&command);
    }

    let link = gtk::LinkButton::with_label(FUSE_DOCS_URL, &t("Learn more"));
    link.set_halign(gtk::Align::Start);
    notice.append(&link);

    Some(notice.upcast())
}

/// Builds the AppImage dependency card shown before the location settings.
fn fuse_dependencies(report: FuseReport, sender: &relm4::ComponentSender<AppModel>) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    card.add_css_class("fuse-status-bar");
    card.set_valign(gtk::Align::Center);
    card.set_tooltip_text(Some(&t(
        "FUSE lets AppImages run without extracting them. FUSE 2 is only needed by older AppImages.",
    )));

    let title = gtk::Label::new(Some(&t("AppImage dependencies")));
    title.add_css_class("heading");
    title.set_xalign(0.0);
    card.append(&title);

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    card.append(&spacer);

    let fuse3 = if report.has_fuse3_helper() {
        FusePresence::Installed
    } else {
        FusePresence::Missing
    };
    let fuse2 = if report.has_libfuse2() && report.has_fuse2_helper() {
        FusePresence::Installed
    } else {
        FusePresence::Missing
    };

    card.append(&fuse_status_item(&t("FUSE 3"), fuse3));
    card.append(&fuse_status_item(&t("FUSE 2"), fuse2));

    let refresh = gtk::Button::with_label(&t("Recheck"));
    refresh.set_valign(gtk::Align::Center);
    refresh.add_css_class("flat");
    refresh.set_tooltip_text(Some(&t("Recheck dependencies")));
    let sender_refresh = sender.clone();
    refresh.connect_clicked(move |_| sender_refresh.input(Msg::WelcomeRecheckDependencies));
    card.append(&refresh);

    card
}

/// Builds one FUSE status item with the same check used by the token status.
fn fuse_status_item(label: &str, presence: FusePresence) -> gtk::Box {
    let item = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    item.set_valign(gtk::Align::Center);

    let installed = presence == FusePresence::Installed;
    let icon = gtk::Image::from_icon_name(if installed {
        "object-select-symbolic"
    } else {
        "dialog-warning-symbolic"
    });
    icon.set_pixel_size(16);
    icon.set_valign(gtk::Align::Center);
    icon.add_css_class(if installed { "success" } else { "warning" });
    item.append(&icon);

    let label = gtk::Label::new(Some(label));
    label.add_css_class("dim-label");
    item.append(&label);

    let tooltip = if installed {
        t("Detected")
    } else {
        t("Not detected")
    };
    item.set_tooltip_text(Some(&tooltip));

    item
}

/// Returns the application icon when the theme can find it.
fn application_icon(window: &adw::Window) -> &'static str {
    let display = gtk::prelude::WidgetExt::display(window);
    let theme = gtk::IconTheme::for_display(&display);
    if theme.has_icon(self_install::ICON_NAME) {
        self_install::ICON_NAME
    } else {
        "system-software-install-symbolic"
    }
}
