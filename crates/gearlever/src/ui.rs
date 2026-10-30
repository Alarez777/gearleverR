//! GTK4 and libadwaita shell for Gear Lever R.
//!
//! The window and the application list arrive next; this module currently owns
//! the application object and the version banner shown while there is no data.

use relm4::adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::settings::APP_ID;

/// Starts the graphical application.
pub(crate) fn run() {
    tracing::info!(
        gtk = format_args!(
            "{}.{}.{}",
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version()
        ),
        "starting the Gear Lever R interface"
    );

    RelmApp::new(APP_ID).run::<AppModel>(());
}

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
