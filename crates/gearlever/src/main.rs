//! Gear Lever R: a GTK4 front-end for managing AppImages.
//!
//! This phase only proves the Relm4/libadwaita shell; the app list and the
//! details view arrive with the core integration.

use relm4::adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

/// Application ID shared by the binary, the desktop entry and GSettings.
const APP_ID: &str = "io.github.alarez777.GearLeverR";

#[derive(Debug, Default)]
struct AppModel;

#[relm4::component]
impl SimpleComponent for AppModel {
    type Init = ();
    type Input = ();
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("Gear Lever R"),
            set_default_width: 700,
            set_default_height: 700,

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,

                adw::HeaderBar {},

                adw::StatusPage {
                    set_icon_name: Some("application-x-executable-symbolic"),
                    set_title: "Gear Lever R",
                    set_description: Some(concat!(
                        "AppImage manager, rewritten in Rust — version ",
                        env!("CARGO_PKG_VERSION"),
                    )),
                    set_vexpand: true,
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let widgets = view_output!();
        ComponentParts {
            model: AppModel,
            widgets,
        }
    }
}

fn main() {
    tracing::info!(
        gtk = format_args!(
            "{}.{}.{}",
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version()
        ),
        "starting Gear Lever R"
    );

    RelmApp::new(APP_ID).run::<AppModel>(());
}
