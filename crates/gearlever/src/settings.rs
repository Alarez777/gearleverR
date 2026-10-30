#![expect(
    dead_code,
    unreachable_pub,
    reason = "this is a private module of the binary; the UI uses these items"
)]

//! Application settings stored in GSettings.
//!
//! The schema keeps the keys of the original Gear Lever so a migration only has
//! to copy values. When the schema is not installed (a source checkout without
//! `glib-compile-schemas`), Gear Lever R falls back to the same defaults.

use std::path::{Path, PathBuf};

use gearlever_core::paths::Paths;
use relm4::gtk::gio;
use relm4::gtk::gio::prelude::*;

/// Application ID, shared by D-Bus, GSettings and the desktop entry.
pub const APP_ID: &str = "io.github.alarez777.GearLeverR";

/// Keys used in the GSettings schema.
pub mod keys {
    /// Folder where AppImages are kept.
    pub const APPIMAGES_FOLDER: &str = "appimages-default-folder";
    /// Whether integrations outside the managed folder are listed.
    pub const MANAGE_OUTSIDE: &str = "manage-files-outside-default-folder";
    /// Whether the original file is removed after integration.
    pub const MOVE_ON_INTEGRATION: &str = "move-appimage-on-integration";
    /// Whether terminal apps are named after their executable.
    pub const EXEC_AS_NAME: &str = "exec-as-name-for-terminal-apps";
    /// Whether new files are shown in a minimal window.
    pub const PREVIEW_MINIMAL_UI: &str = "preview-minimal-ui";
    /// Whether `appimage-extract` is blocked as an extractor.
    pub const BLOCK_UNSAFE_EXTRACTOR: &str = "block-unsafe-extractor";
    /// Whether updates are checked at login.
    pub const FETCH_IN_BACKGROUND: &str = "fetch-updates-in-background";
    /// Verbose logging.
    pub const DEBUG_LOGS: &str = "debug-logs";
    /// Whether the window was maximized.
    pub const IS_MAXIMIZED: &str = "is-maximized";
    /// Whether the welcome dialog was shown.
    pub const FIRST_RUN: &str = "first-run";
}

/// Plain snapshot of the settings, used by code that does not need GSettings.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the GSettings schema itself is a bag of booleans"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Values {
    /// Folder where AppImages are kept, with `~` expanded.
    pub appimages_dir: PathBuf,
    /// Whether integrations outside the managed folder are listed.
    pub manage_outside: bool,
    /// Whether the original file is removed after integration.
    pub move_on_integration: bool,
    /// Whether terminal apps are named after their executable.
    pub exec_name_terminal: bool,
    /// Whether new files are shown in a minimal window.
    pub preview_minimal_ui: bool,
    /// Whether `appimage-extract` is blocked as an extractor.
    pub block_unsafe_extractor: bool,
    /// Whether updates are checked at login.
    pub fetch_in_background: bool,
    /// Verbose logging.
    pub debug_logs: bool,
    /// Whether the window was maximized.
    pub is_maximized: bool,
    /// Whether the welcome dialog was shown.
    pub first_run: bool,
}

impl Values {
    fn defaults(paths: &Paths) -> Self {
        Self {
            appimages_dir: paths.default_appimages_dir(),
            manage_outside: true,
            move_on_integration: true,
            exec_name_terminal: false,
            preview_minimal_ui: true,
            block_unsafe_extractor: true,
            fetch_in_background: false,
            debug_logs: false,
            is_maximized: false,
            first_run: true,
        }
    }
}

/// GSettings-backed settings with a plain snapshot.
#[derive(Debug, Clone)]
pub struct Settings {
    settings: Option<gio::Settings>,
    values: Values,
}

impl Settings {
    /// Loads the settings for this user.
    #[must_use]
    pub fn load(paths: &Paths) -> Self {
        let schema =
            gio::SettingsSchemaSource::default().and_then(|source| source.lookup(APP_ID, true));

        let Some(_schema) = schema else {
            tracing::debug!("GSettings schema not found, using defaults");
            return Self {
                settings: None,
                values: Values::defaults(paths),
            };
        };

        let settings = gio::Settings::new(APP_ID);
        let values = Values {
            appimages_dir: expand(
                settings.string(keys::APPIMAGES_FOLDER).as_str(),
                paths.home(),
            ),
            manage_outside: settings.boolean(keys::MANAGE_OUTSIDE),
            move_on_integration: settings.boolean(keys::MOVE_ON_INTEGRATION),
            exec_name_terminal: settings.boolean(keys::EXEC_AS_NAME),
            preview_minimal_ui: settings.boolean(keys::PREVIEW_MINIMAL_UI),
            block_unsafe_extractor: settings.boolean(keys::BLOCK_UNSAFE_EXTRACTOR),
            fetch_in_background: settings.boolean(keys::FETCH_IN_BACKGROUND),
            debug_logs: settings.boolean(keys::DEBUG_LOGS),
            is_maximized: settings.boolean(keys::IS_MAXIMIZED),
            first_run: settings.boolean(keys::FIRST_RUN),
        };

        Self {
            settings: Some(settings),
            values,
        }
    }

    /// Returns the current values.
    #[must_use]
    pub fn values(&self) -> &Values {
        &self.values
    }

    /// Returns whether GSettings is usable.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.settings.is_some()
    }

    /// Stores a boolean, keeping the snapshot in sync.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "GSettings booleans are inherently bool"
    )]
    pub fn set_boolean(&mut self, key: &str, value: bool) {
        if let Some(settings) = &self.settings {
            let _ = settings.set_boolean(key, value);
        }

        match key {
            keys::MANAGE_OUTSIDE => self.values.manage_outside = value,
            keys::MOVE_ON_INTEGRATION => self.values.move_on_integration = value,
            keys::EXEC_AS_NAME => self.values.exec_name_terminal = value,
            keys::PREVIEW_MINIMAL_UI => self.values.preview_minimal_ui = value,
            keys::BLOCK_UNSAFE_EXTRACTOR => self.values.block_unsafe_extractor = value,
            keys::FETCH_IN_BACKGROUND => self.values.fetch_in_background = value,
            keys::DEBUG_LOGS => self.values.debug_logs = value,
            keys::IS_MAXIMIZED => self.values.is_maximized = value,
            keys::FIRST_RUN => self.values.first_run = value,
            _ => {}
        }
    }

    /// Stores the managed folder.
    pub fn set_appimages_dir(&mut self, path: &Path) {
        if let Some(settings) = &self.settings {
            let _ = settings.set_string(keys::APPIMAGES_FOLDER, &path.to_string_lossy());
        }
        self.values.appimages_dir = path.to_path_buf();
    }
}

/// Expands a leading `~` using the user's home directory.
fn expand(value: &str, home: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix('~') {
        home.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde() {
        let home = Path::new("/home/tester");

        assert_eq!(
            expand("~/AppImages", home),
            PathBuf::from("/home/tester/AppImages")
        );
        assert_eq!(expand("~/", home), PathBuf::from("/home/tester"));
        assert_eq!(expand("/opt/apps", home), PathBuf::from("/opt/apps"));
    }
}
