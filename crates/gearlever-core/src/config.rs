//! Gear Lever-compatible configuration file.
//!
//! The original stores per-application settings in `gearlever.conf` using
//! `configparser`: one `[app.<md5(path)>]` section per AppImage plus an optional
//! `[app.<md5>.update_manager]` section. Gear LeverR writes the same format to
//! its own file so existing integrations can be imported without data loss.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use md5::{Digest, Md5};

use crate::error::{Error, Result};
use crate::paths::Paths;

/// Section used for global, non per-application settings.
const DEFAULT_SECTION: &str = "DEFAULT";

/// Per-application key that disables update checks.
const SKIP_UPDATES_KEY: &str = "skip_updates";

/// Prefix of the per-application keys that keep disabled environment variables.
const DISABLED_ENV_PREFIX: &str = "disabled_env_";

/// Global section that remembers environment variables used by any application.
const ENVIRONMENT_HISTORY_SECTION: &str = "environment_history";

/// Prefix of the keys inside [`ENVIRONMENT_HISTORY_SECTION`].
const ENVIRONMENT_HISTORY_PREFIX: &str = "entry_";

/// Maximum number of environment variables kept in the history.
const ENVIRONMENT_HISTORY_LIMIT: usize = 100;

/// An INI document with the same semantics as the original configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl Config {
    /// Loads a configuration file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the file cannot be read.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)?;
        Ok(Self::parse(&contents))
    }

    /// Loads a configuration file, returning an empty document when missing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for errors other than a missing file.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        match Self::load(path) {
            Ok(config) => Ok(config),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    /// Parses INI contents.
    #[must_use]
    pub fn parse(contents: &str) -> Self {
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current: Option<String> = None;

        for line in contents.lines() {
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let name = name.trim().to_owned();
                sections.entry(name.clone()).or_default();
                current = Some(name);
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            if let Some(section) = current.as_deref() {
                // configparser lower-cases option names by default.
                sections
                    .entry(section.to_owned())
                    .or_default()
                    .insert(key.trim().to_lowercase(), value.trim().to_owned());
            }
        }

        Self { sections }
    }

    /// Returns the raw contents that [`Config::save`] would write.
    #[must_use]
    pub fn to_ini_string(&self) -> String {
        let mut output = String::new();
        let mut first = true;

        // The global section comes first, like configparser writes it.
        if let Some(values) = self.sections.get(DEFAULT_SECTION) {
            write_section(&mut output, DEFAULT_SECTION, values, &mut first);
        }

        for (name, values) in &self.sections {
            if name == DEFAULT_SECTION {
                continue;
            }
            write_section(&mut output, name, values, &mut first);
        }

        output
    }

    /// Writes the configuration atomically, creating its directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory or file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let temporary = path.with_extension("conf.tmp");
        fs::write(&temporary, self.to_ini_string())?;
        fs::rename(&temporary, path)?;

        Ok(())
    }

    /// Returns one value.
    #[must_use]
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(section)
            .and_then(|values| values.get(key))
            .map(String::as_str)
    }

    /// Returns a boolean stored as `yes`/`no`, `true`/`false` or `1`/`0`.
    #[must_use]
    pub fn get_boolean(&self, section: &str, key: &str) -> Option<bool> {
        self.get(section, key).map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "yes" | "true" | "1" | "on"
            )
        })
    }

    /// Sets one value.
    pub fn set(&mut self, section: &str, key: &str, value: impl Into<String>) {
        self.sections
            .entry(section.to_owned())
            .or_default()
            .insert(key.to_lowercase(), value.into());
    }

    /// Removes a whole section.
    pub fn remove_section(&mut self, section: &str) {
        self.sections.remove(section);
    }

    /// Returns the section that belongs to an AppImage.
    #[must_use]
    pub fn app(&self, image: &Path) -> BTreeMap<String, String> {
        self.sections
            .get(&app_section(image))
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces the section that belongs to an AppImage.
    pub fn set_app(&mut self, image: &Path, values: BTreeMap<String, String>) {
        self.sections.insert(app_section(image), values);
    }

    /// Removes both the application and its update manager sections.
    pub fn delete_app(&mut self, image: &Path) {
        let section = app_section(image);
        self.sections.remove(&section);
        self.sections.remove(&format!("{section}.update_manager"));
    }

    /// Returns whether update checks are disabled for `image`.
    #[must_use]
    pub fn updates_disabled(&self, image: &Path) -> bool {
        self.get_boolean(&app_section(image), SKIP_UPDATES_KEY) == Some(true)
    }

    /// Disables update checks for `image`.
    pub fn disable_updates(&mut self, image: &Path) {
        let mut values = self.app(image);
        values.insert(SKIP_UPDATES_KEY.to_owned(), "true".to_owned());
        self.set_app(image, values);
    }

    /// Enables update checks for `image`.
    pub fn enable_updates(&mut self, image: &Path) {
        let mut values = self.app(image);
        values.remove(SKIP_UPDATES_KEY);
        self.set_app(image, values);
    }

    /// Returns the environment variables disabled for `image`, in saved order.
    ///
    /// Each entry keeps the `KEY=value` form used by the editor, so re-enabling
    /// one only has to quote the value again for the desktop entry.
    #[must_use]
    pub fn disabled_env(&self, image: &Path) -> Vec<String> {
        let Some(values) = self.sections.get(&app_section(image)) else {
            return Vec::new();
        };

        let mut entries: Vec<(usize, &str)> = values
            .iter()
            .filter_map(|(key, value)| {
                let index = key.strip_prefix(DISABLED_ENV_PREFIX)?.parse().ok()?;
                Some((index, value.as_str()))
            })
            .collect();
        entries.sort_by_key(|(index, _)| *index);

        entries
            .into_iter()
            .map(|(_, value)| value.to_owned())
            .collect()
    }

    /// Replaces the environment variables disabled for `image`.
    ///
    /// Disabled variables stay in the configuration while they are absent from
    /// the desktop entry, so the editor can offer them again.
    pub fn set_disabled_env(&mut self, image: &Path, variables: &[String]) {
        let mut values = self.app(image);
        values.retain(|key, _| !key.starts_with(DISABLED_ENV_PREFIX));

        for (index, variable) in variables.iter().enumerate() {
            values.insert(format!("{DISABLED_ENV_PREFIX}{index}"), variable.clone());
        }

        if values.is_empty() {
            self.sections.remove(&app_section(image));
        } else {
            self.set_app(image, values);
        }
    }

    /// Returns the update manager configuration of an AppImage.
    #[must_use]
    pub fn update_manager(&self, image: &Path) -> BTreeMap<String, String> {
        self.sections
            .get(&update_section(image))
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces the update manager configuration of an AppImage.
    pub fn set_update_manager(
        &mut self,
        image: &Path,
        manager: &str,
        mut values: BTreeMap<String, String>,
    ) {
        values.insert("manager".to_owned(), manager.to_owned());
        self.sections.insert(update_section(image), values);
    }

    /// Removes the update manager configuration of an AppImage.
    pub fn delete_update_manager(&mut self, image: &Path) {
        self.sections.remove(&update_section(image));
    }

    /// Stores a global setting.
    pub fn set_default(&mut self, key: &str, value: impl Into<String>) {
        self.set(DEFAULT_SECTION, key, value);
    }

    /// Returns a global setting.
    #[must_use]
    pub fn default_value(&self, key: &str) -> Option<&str> {
        self.get(DEFAULT_SECTION, key)
    }

    /// Returns the environment variables remembered across applications.
    ///
    /// Entries keep the `KEY=value` form and are ordered from most to least
    /// recently used.
    #[must_use]
    pub fn environment_history(&self) -> Vec<String> {
        let Some(values) = self.sections.get(ENVIRONMENT_HISTORY_SECTION) else {
            return Vec::new();
        };

        let mut entries: Vec<(usize, &str)> = values
            .iter()
            .filter_map(|(key, value)| {
                let index = key.strip_prefix(ENVIRONMENT_HISTORY_PREFIX)?.parse().ok()?;
                Some((index, value.as_str()))
            })
            .collect();
        entries.sort_by_key(|(index, _)| *index);

        entries
            .into_iter()
            .map(|(_, value)| value.to_owned())
            .collect()
    }

    /// Moves `variables` to the front of the environment history.
    ///
    /// Returns whether the stored history changed.
    pub fn remember_environment(&mut self, variables: &[String]) -> bool {
        let previous = self.environment_history();
        let mut merged: Vec<String> = Vec::new();

        for variable in variables.iter().chain(previous.iter()) {
            if !variable.is_empty() && !merged.contains(variable) {
                merged.push(variable.clone());
            }
        }
        merged.truncate(ENVIRONMENT_HISTORY_LIMIT);

        if merged == previous {
            return false;
        }

        self.sections.remove(ENVIRONMENT_HISTORY_SECTION);
        for (index, variable) in merged.iter().enumerate() {
            self.set(
                ENVIRONMENT_HISTORY_SECTION,
                &format!("{ENVIRONMENT_HISTORY_PREFIX}{index:03}"),
                variable.clone(),
            );
        }

        true
    }

    /// Imports the configuration written by the original Gear Lever.
    ///
    /// Returns `true` when a legacy file was found and copied. An existing
    /// Gear LeverR configuration is never overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when either file cannot be read or written.
    pub fn import_legacy(paths: &Paths) -> Result<bool> {
        let target = paths.config_file();

        if target.exists() {
            return Ok(false);
        }

        let legacy = paths.legacy_config_file();
        if !legacy.exists() {
            return Ok(false);
        }

        let config = Self::load(&legacy)?;
        config.save(&target)?;

        Ok(true)
    }
}

/// Returns the section name for an AppImage, matching the original hash.
#[must_use]
pub fn app_section(image: &Path) -> String {
    format!("app.{}", md5_hex(&image.to_string_lossy()))
}

fn update_section(image: &Path) -> String {
    format!("{}.update_manager", app_section(image))
}

fn md5_hex(value: &str) -> String {
    let digest = Md5::digest(value.as_bytes());
    let mut hex = String::with_capacity(32);

    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }

    hex
}

fn write_section(
    output: &mut String,
    name: &str,
    values: &BTreeMap<String, String>,
    first: &mut bool,
) {
    if !*first {
        output.push('\n');
    }
    let _ = writeln!(output, "[{name}]");

    for (key, value) in values {
        let _ = writeln!(output, "{key} = {value}");
    }

    *first = false;
}

/// Creates a configuration path below a temporary root, for tests.
#[cfg(test)]
fn temp_config_path() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gearlever-r.conf");
    (dir, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_and_application_sections() {
        let config = Config::parse(
            "[DEFAULT]\nfetch-updates-in-background = yes\n\n\
             [app.abc]\nname = Example\nfile_path = /home/x/AppImages/example.appimage\n",
        );

        assert_eq!(
            config.default_value("fetch-updates-in-background"),
            Some("yes")
        );
        assert_eq!(config.get("app.abc", "name"), Some("Example"));
        assert_eq!(
            config.get_boolean("DEFAULT", "fetch-updates-in-background"),
            Some(true)
        );
    }

    #[test]
    fn lower_cases_keys_like_configparser() {
        let config = Config::parse("[app.abc]\nAllow_PreReleases = False\n");

        assert_eq!(config.get("app.abc", "allow_prereleases"), Some("False"));
    }

    #[test]
    fn uses_md5_of_the_path_as_the_section_key() {
        assert_eq!(
            app_section(Path::new("hello")),
            "app.5d41402abc4b2a76b9719d911017c592"
        );
    }

    #[test]
    fn app_and_update_manager_sections_round_trip() {
        let (_dir, path) = temp_config_path();
        let image = Path::new("/home/x/AppImages/example.appimage");

        let mut config = Config::default();
        config.set_default("fetch-updates-in-background", "no");
        config.set_app(
            image,
            BTreeMap::from([
                ("name".to_owned(), "Example".to_owned()),
                ("file_path".to_owned(), image.to_string_lossy().into_owned()),
            ]),
        );
        config.set_update_manager(
            image,
            "GithubUpdater",
            BTreeMap::from([("repo".to_owned(), "owner/repo".to_owned())]),
        );
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(
            loaded
                .update_manager(image)
                .get("manager")
                .map(String::as_str),
            Some("GithubUpdater")
        );
        assert_eq!(
            loaded.update_manager(image).get("repo").map(String::as_str),
            Some("owner/repo")
        );
    }

    #[test]
    fn deleting_an_app_removes_its_update_manager_too() {
        let image = Path::new("/home/x/AppImages/example.appimage");
        let mut config = Config::default();
        config.set_app(image, BTreeMap::new());
        config.set_update_manager(image, "StaticFileUpdater", BTreeMap::new());

        config.delete_app(image);

        assert!(config.app(image).is_empty());
        assert!(config.update_manager(image).is_empty());
        assert!(!config.to_ini_string().contains(".update_manager"));
    }

    #[test]
    fn stores_the_per_app_update_switch() {
        let image = Path::new("/home/x/AppImages/example.appimage");
        let mut config = Config::default();
        assert!(!config.updates_disabled(image));

        config.disable_updates(image);
        assert!(config.updates_disabled(image));
        assert_eq!(
            config.app(image).get("skip_updates").map(String::as_str),
            Some("true")
        );

        config.enable_updates(image);
        assert!(!config.updates_disabled(image));
        assert!(!config.app(image).contains_key("skip_updates"));
    }

    #[test]
    fn parses_the_per_app_update_switch() {
        let image = Path::new("/home/x/AppImages/example.appimage");
        let section = app_section(image);
        let config = Config::parse(&format!("[{section}]\nskip_updates = yes\n"));

        assert!(config.updates_disabled(image));
    }

    #[test]
    fn stores_disabled_environment_variables_in_order() {
        let image = Path::new("/home/x/AppImages/example.appimage");
        let mut config = Config::default();
        config.set(&app_section(image), "name", "Example");
        assert!(config.disabled_env(image).is_empty());

        let variables = vec!["FIRST=1".to_owned(), "SECOND=with spaces".to_owned()];
        config.set_disabled_env(image, &variables);

        assert_eq!(config.disabled_env(image), variables);
        assert_eq!(
            config.get(&app_section(image), "disabled_env_0"),
            Some("FIRST=1")
        );
        assert_eq!(config.get(&app_section(image), "name"), Some("Example"));

        config.set_disabled_env(image, &[]);
        assert!(config.disabled_env(image).is_empty());
        assert!(!config.app(image).contains_key("disabled_env_0"));
        assert_eq!(config.get(&app_section(image), "name"), Some("Example"));
    }

    #[test]
    fn parses_disabled_environment_variables_by_index() {
        let image = Path::new("/home/x/AppImages/example.appimage");
        let section = app_section(image);
        let config = Config::parse(&format!(
            "[{section}]\ndisabled_env_1 = B=2\ndisabled_env_0 = A=1\nname = Example\n"
        ));

        assert_eq!(config.disabled_env(image), vec!["A=1", "B=2"]);
        assert_eq!(config.get(&section, "name"), Some("Example"));
    }

    #[test]
    fn remembers_environment_variables_in_recent_order() {
        let mut config = Config::default();
        assert!(config.environment_history().is_empty());

        let first = vec!["A=1".to_owned(), "B=2".to_owned()];
        assert!(config.remember_environment(&first));
        assert_eq!(config.environment_history(), first);
        assert!(!config.remember_environment(&first));

        let second = vec!["B=2".to_owned(), "C=3".to_owned()];
        assert!(config.remember_environment(&second));
        assert_eq!(config.environment_history(), vec!["B=2", "C=3", "A=1"]);
        assert_eq!(
            config.get(ENVIRONMENT_HISTORY_SECTION, "entry_000"),
            Some("B=2")
        );
    }

    #[test]
    fn caps_the_environment_history() {
        let mut config = Config::default();
        let variables: Vec<String> = (0..105).map(|index| format!("KEY{index}=value")).collect();

        config.remember_environment(&variables);

        let history = config.environment_history();
        assert_eq!(history.len(), ENVIRONMENT_HISTORY_LIMIT);
        assert_eq!(history.first().map(String::as_str), Some("KEY0=value"));
        assert_eq!(history.last().map(String::as_str), Some("KEY99=value"));
    }

    #[test]
    fn parses_the_environment_history_by_index() {
        let config =
            Config::parse("[environment_history]\nentry_001 = B=2\nentry_000 = A=with = signs\n");

        assert_eq!(config.environment_history(), vec!["A=with = signs", "B=2"]);
    }

    #[test]
    fn imports_the_legacy_file_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::write(
            paths.legacy_config_file(),
            "[DEFAULT]\nfetch-updates-in-background = yes\n",
        )
        .unwrap();

        assert!(Config::import_legacy(&paths).unwrap());
        assert!(paths.config_file().exists());
        assert!(
            !Config::import_legacy(&paths).unwrap(),
            "must not overwrite"
        );
    }
}
