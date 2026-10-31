//! First-run window that integrates Gear LeverR into the desktop.
//!
//! The window is shown while Gear LeverR is running from an AppImage that has no
//! usable desktop entry of its own. It offers the settings that matter before the
//! first integration and writes the entry with [`gearlever_core::self_install`].

#![expect(unreachable_pub, reason = "private module of the binary")]

use std::cell::Cell;
use std::rc::Rc;

use gearlever_core::self_install;
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::t;
use crate::settings::Settings;

use super::{AppModel, Msg};

/// Widgets of the first-run window that the model keeps updating.
pub(crate) struct Welcome {
    pub window: adw::Window,
    pub toast: adw::ToastOverlay,
    pub folder_row: adw::ActionRow,
    pub install_button: gtk::Button,
    pub skip_button: gtk::Button,
    pub close_button: gtk::Button,
    pub spinner: gtk::Spinner,
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
        self.install_button.set_sensitive(true);
        self.skip_button.set_sensitive(true);
        self.close_button.set_sensitive(true);
    }

    fn disable_controls(&self) {
        self.install_button.set_sensitive(false);
        self.skip_button.set_sensitive(false);
        self.close_button.set_sensitive(false);
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
    let window = adw::Window::builder()
        .title(t("Welcome to Gear LeverR"))
        .transient_for(parent)
        .modal(true)
        .default_width(620)
        .default_height(680)
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
    intro_text.set_max_width_chars(48);

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
    body.set_margin_top(12);
    body.set_margin_bottom(24);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.append(&intro);
    body.append(&location);
    body.append(&files);
    body.append(&updates);

    let clamp = adw::Clamp::builder().maximum_size(560).child(&body).build();
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

    Welcome {
        window,
        toast,
        folder_row,
        install_button,
        skip_button,
        close_button,
        spinner,
        fetch_switch,
        fetch_guard,
    }
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
