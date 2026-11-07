#![expect(
    dead_code,
    unreachable_pub,
    reason = "this is a private module of the binary; the UI uses these items"
)]

//! Application settings stored in GSettings.
//!
//! The schema keeps the keys of the original Gear Lever so a migration only has
//! to copy values. When the schema is not installed (a source checkout without
//! `glib-compile-schemas`), Gear LeverR falls back to the same defaults.

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
    /// Whether update sources that publish zsync data use delta downloads.
    pub const USE_ZSYNC_DELTA: &str = "use-zsync-delta";
    /// Verbose logging.
    pub const DEBUG_LOGS: &str = "debug-logs";
    /// Whether the window was maximized.
    pub const IS_MAXIMIZED: &str = "is-maximized";
    /// Last normal window width.
    pub const WINDOW_WIDTH: &str = "window-width";
    /// Last normal window height.
    pub const WINDOW_HEIGHT: &str = "window-height";
    /// Whether the welcome dialog was shown.
    pub const FIRST_RUN: &str = "first-run";
    /// Percentage used to scale text and buttons.
    pub const UI_SCALE: &str = "ui-scale";
    /// Encrypted GitHub token used when no Secret Service is available.
    pub const GITHUB_TOKEN_CIPHER: &str = "github-token-cipher";
    /// Whether a GitHub token is configured and whether it was verified.
    pub const GITHUB_TOKEN_STATUS: &str = "github-token-status";
    /// GitHub token expiration header value, when available.
    pub const GITHUB_TOKEN_EXPIRES: &str = "github-token-expires";
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
    /// Whether update sources that publish zsync data use delta downloads.
    pub use_zsync_delta: bool,
    /// Verbose logging.
    pub debug_logs: bool,
    /// Whether the window was maximized.
    pub is_maximized: bool,
    /// Last normal window width.
    pub window_width: i32,
    /// Last normal window height.
    pub window_height: i32,
    /// Whether the welcome dialog was shown.
    pub first_run: bool,
    /// Percentage used to scale text and buttons.
    pub ui_scale: u32,
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
            use_zsync_delta: true,
            debug_logs: false,
            is_maximized: false,
            window_width: 720,
            window_height: 720,
            first_run: true,
            ui_scale: 100,
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
        Self::from_settings(settings, paths)
    }

    fn from_settings(settings: gio::Settings, paths: &Paths) -> Self {
        // A newer binary may run against an older installed schema. Reading a
        // missing key emits a GLib critical, so fall back to the documented
        // default instead.
        let use_zsync_delta = settings
            .settings_schema()
            .is_none_or(|schema| !schema.has_key(keys::USE_ZSYNC_DELTA))
            || settings.boolean(keys::USE_ZSYNC_DELTA);
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
            use_zsync_delta,
            debug_logs: settings.boolean(keys::DEBUG_LOGS),
            is_maximized: settings.boolean(keys::IS_MAXIMIZED),
            window_width: integer_value(&settings, keys::WINDOW_WIDTH, 720).clamp(320, 10_000),
            window_height: integer_value(&settings, keys::WINDOW_HEIGHT, 720).clamp(240, 10_000),
            first_run: settings.boolean(keys::FIRST_RUN),
            ui_scale: ui_scale(&settings),
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
            keys::USE_ZSYNC_DELTA => self.values.use_zsync_delta = value,
            keys::DEBUG_LOGS => self.values.debug_logs = value,
            keys::IS_MAXIMIZED => self.values.is_maximized = value,
            keys::FIRST_RUN => self.values.first_run = value,
            _ => {}
        }
    }

    /// Writes a boolean to GSettings without touching the snapshot.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "GSettings booleans are inherently bool"
    )]
    pub fn store_boolean(&self, key: &str, value: bool) {
        if let Some(settings) = &self.settings {
            let _ = settings.set_boolean(key, value);
        }
    }

    /// Writes the managed folder to GSettings without touching the snapshot.
    pub fn store_appimages_dir(&self, path: &Path) {
        if let Some(settings) = &self.settings {
            let _ = settings.set_string(keys::APPIMAGES_FOLDER, &path.to_string_lossy());
        }
    }

    /// Stores the last normal window size.
    pub fn store_window_size(&self, width: i32, height: i32) {
        let width = width.clamp(320, 10_000);
        let height = height.clamp(240, 10_000);
        let Some(settings) = &self.settings else {
            return;
        };
        let Some(schema) = settings.settings_schema() else {
            return;
        };

        if schema.has_key(keys::WINDOW_WIDTH) {
            let _ = settings.set_int(keys::WINDOW_WIDTH, width);
        }
        if schema.has_key(keys::WINDOW_HEIGHT) {
            let _ = settings.set_int(keys::WINDOW_HEIGHT, height);
        }
    }

    /// Stores the interface scale percentage, keeping the snapshot in sync.
    pub fn set_ui_scale(&mut self, percent: u32) {
        let percent = percent.clamp(100, 200);

        if let Some(settings) = &self.settings
            && settings
                .settings_schema()
                .is_some_and(|schema| schema.has_key(keys::UI_SCALE))
        {
            let _ = settings.set_int(keys::UI_SCALE, i32::try_from(percent).unwrap_or(100));
        }

        self.values.ui_scale = percent;
    }

    /// Returns the encrypted GitHub token blob, when one is stored.
    #[must_use]
    pub fn github_token_cipher(&self) -> Option<String> {
        self.string_value(keys::GITHUB_TOKEN_CIPHER)
    }

    /// Stores the encrypted GitHub token blob and returns whether it persisted.
    pub fn set_github_token_cipher(&self, value: &str) -> bool {
        self.set_string_value(keys::GITHUB_TOKEN_CIPHER, value)
    }

    /// Returns the stored GitHub token status.
    #[must_use]
    pub fn github_token_status(&self) -> Option<String> {
        self.string_value(keys::GITHUB_TOKEN_STATUS)
    }

    /// Stores the GitHub token status and returns whether it persisted.
    pub fn set_github_token_status(&self, value: &str) -> bool {
        self.set_string_value(keys::GITHUB_TOKEN_STATUS, value)
    }

    /// Returns the stored GitHub token expiration.
    #[must_use]
    pub fn github_token_expires(&self) -> Option<String> {
        self.string_value(keys::GITHUB_TOKEN_EXPIRES)
    }

    /// Stores the GitHub token expiration and returns whether it persisted.
    pub fn set_github_token_expires(&self, value: &str) -> bool {
        self.set_string_value(keys::GITHUB_TOKEN_EXPIRES, value)
    }

    fn string_value(&self, key: &str) -> Option<String> {
        let settings = self.settings.as_ref()?;
        let schema = settings.settings_schema()?;
        if !schema.has_key(key) {
            return None;
        }

        let value = settings.string(key);
        (!value.is_empty()).then(|| value.to_string())
    }

    fn set_string_value(&self, key: &str, value: &str) -> bool {
        let Some(settings) = &self.settings else {
            return false;
        };
        let Some(schema) = settings.settings_schema() else {
            return false;
        };
        if !schema.has_key(key) {
            return false;
        }

        settings.set_string(key, value).is_ok()
    }

    /// Stores the managed folder.
    pub fn set_appimages_dir(&mut self, path: &Path) {
        if let Some(settings) = &self.settings {
            let _ = settings.set_string(keys::APPIMAGES_FOLDER, &path.to_string_lossy());
        }
        self.values.appimages_dir = path.to_path_buf();
    }
}

/// Copies the settings of the original Gear Lever once.
///
/// The schema keeps the same keys, so migration is a straight copy. A marker in
/// the configuration directory prevents later runs from overwriting changes.
fn migrate_legacy(
    settings: &gio::Settings,
    source: Option<&gio::SettingsSchemaSource>,
    paths: &Paths,
) {
    let marker = paths.config_dir().join("gearlever-r.migrated");
    if marker.exists() {
        return;
    }

    if source
        .and_then(|source| source.lookup("it.mijorus.gearlever", true))
        .is_none()
    {
        return;
    }

    let legacy = gio::Settings::new("it.mijorus.gearlever");

    let _ = settings.set_string(
        keys::APPIMAGES_FOLDER,
        legacy.string(keys::APPIMAGES_FOLDER).as_str(),
    );

    for key in [
        keys::MANAGE_OUTSIDE,
        keys::MOVE_ON_INTEGRATION,
        keys::EXEC_AS_NAME,
        keys::PREVIEW_MINIMAL_UI,
        keys::BLOCK_UNSAFE_EXTRACTOR,
        keys::FETCH_IN_BACKGROUND,
        keys::DEBUG_LOGS,
        keys::IS_MAXIMIZED,
        keys::FIRST_RUN,
    ] {
        let _ = settings.set_boolean(key, legacy.boolean(key));
    }

    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::write(marker, b"1\n");
    }
}

/// Returns an integer setting, falling back when the installed schema predates
/// the key.
fn integer_value(settings: &gio::Settings, key: &str, default: i32) -> i32 {
    let Some(schema) = settings.settings_schema() else {
        return default;
    };

    if schema.has_key(key) {
        settings.int(key)
    } else {
        default
    }
}

/// Returns the saved interface scale, falling back to 100 when the installed
/// schema predates the key.
fn ui_scale(settings: &gio::Settings) -> u32 {
    u32::try_from(integer_value(settings, keys::UI_SCALE, 100))
        .unwrap_or(100)
        .clamp(100, 200)
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
    use std::fs;
    use std::process::Command;

    use super::*;

    #[test]
    fn persists_the_zsync_delta_setting_in_gsettings() {
        let directory = tempfile::tempdir().unwrap();
        let schema_dir = directory.path().join("schemas");
        fs::create_dir_all(&schema_dir).unwrap();
        let schema = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/io.github.alarez777.GearLeverR.gschema.xml");
        fs::copy(
            &schema,
            schema_dir.join("io.github.alarez777.GearLeverR.gschema.xml"),
        )
        .unwrap();

        let status = Command::new("glib-compile-schemas")
            .arg("--strict")
            .arg(&schema_dir)
            .status()
            .expect("glib-compile-schemas is required for the settings test");
        assert!(status.success(), "schema compilation failed");

        let source = gio::SettingsSchemaSource::from_directory(&schema_dir, None, true).unwrap();
        let schema = source.lookup(APP_ID, true).unwrap();
        let backend = gio::memory_settings_backend_new();
        let raw = gio::Settings::new_full(&schema, Some(&backend), None);
        let paths = Paths::with_home(Path::new("/home/tester"));

        let mut settings = Settings::from_settings(raw.clone(), &paths);
        assert!(settings.values().use_zsync_delta);
        settings.set_boolean(keys::USE_ZSYNC_DELTA, false);

        let reloaded = Settings::from_settings(raw, &paths);
        assert!(!reloaded.values().use_zsync_delta);
    }

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

    #[test]
    fn defaults_the_window_size() {
        let paths = Paths::with_home(Path::new("/home/tester"));
        let settings = Settings {
            settings: None,
            values: Values::defaults(&paths),
        };

        assert_eq!(settings.values().window_width, 720);
        assert_eq!(settings.values().window_height, 720);
    }

    #[test]
    fn enables_zsync_delta_by_default_and_can_disable_it() {
        let paths = Paths::with_home(Path::new("/home/tester"));
        let mut settings = Settings {
            settings: None,
            values: Values::defaults(&paths),
        };

        assert!(settings.values().use_zsync_delta);
        settings.set_boolean(keys::USE_ZSYNC_DELTA, false);
        assert!(!settings.values().use_zsync_delta);
    }

    #[test]
    fn clamps_the_interface_scale() {
        let paths = Paths::with_home(Path::new("/home/tester"));
        let mut settings = Settings {
            settings: None,
            values: Values::defaults(&paths),
        };

        assert_eq!(settings.values().ui_scale, 100);
        settings.set_ui_scale(250);
        assert_eq!(settings.values().ui_scale, 200);
        settings.set_ui_scale(50);
        assert_eq!(settings.values().ui_scale, 100);
    }
}
