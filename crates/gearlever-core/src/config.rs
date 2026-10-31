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
