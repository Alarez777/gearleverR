//! Welcome dialog shown on first run and from the menu.

#![expect(unreachable_pub, reason = "private module of the binary")]

use relm4::adw::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::t;

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the carousel position is a small non-negative page index"
)]
fn page_index(position: f64) -> u32 {
    position.max(0.0).round() as u32
}

const PAGES: [(&str, &str, &str); 3] = [
    (
        "system-software-install-symbolic",
        "Hi, welcome to Gear LeverR",
        "An app that helps you integrate AppImages into your system.",
    ),
    (
        "folder-symbolic",
        "Set the AppImage location",
        "Gear LeverR groups all your AppImages into a specific folder and keeps them organized.",
    ),
    (
        "object-select-symbolic",
        "All done, let's go!",
        "You can customize it later in the preferences.",
    ),
];

/// Shows the tutorial window.
pub fn present(parent: &adw::ApplicationWindow) {
    let window = adw::Window::new();
    window.set_title(Some(&t("Welcome")));
    window.set_modal(true);
    window.set_transient_for(Some(parent));
    window.set_default_size(560, 460);

    let toolbar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    toolbar.append(&header);

    let carousel = adw::Carousel::new();
    carousel.set_spacing(24);

    for (icon, title, description) in PAGES {
        let page = adw::StatusPage::builder()
            .icon_name(icon)
            .title(t(title))
            .description(t(description))
            .build();
        carousel.append(&page);
    }

    let dots = adw::CarouselIndicatorDots::new();
    dots.set_carousel(Some(&carousel));
    header.set_title_widget(Some(&dots));

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    controls.set_halign(gtk::Align::Center);
    controls.set_margin_top(12);
    controls.set_margin_bottom(18);

    let previous = gtk::Button::from_icon_name("go-previous-symbolic");
    let next = gtk::Button::from_icon_name("go-next-symbolic");
    let close = gtk::Button::with_label(&t("Close tutorial"));
    close.add_css_class("suggested-action");
    controls.append(&previous);
    controls.append(&next);
    controls.append(&close);

    let carousel_previous = carousel.clone();
    previous.connect_clicked(move |_| {
        let position = page_index(carousel_previous.position()).saturating_sub(1);
        carousel_previous.scroll_to(&carousel_previous.nth_page(position), true);
    });

    let carousel_next = carousel.clone();
    next.connect_clicked(move |_| {
        let position = page_index(carousel_next.position()) + 1;
        carousel_next.scroll_to(&carousel_next.nth_page(position), true);
    });

    let window_close = window.clone();
    close.connect_clicked(move |_| window_close.close());

    toolbar.append(&carousel);
    toolbar.append(&controls);
    window.set_content(Some(&toolbar));
    window.present();
}
