//! Minimal reader for freedesktop.org desktop entries.
//!
//! Gear Lever R needs to read entries embedded in AppImages and to rewrite the
//! integrated copy while preserving unknown keys and `[Desktop Action ...]`
//! groups. This module keeps the file as a map of groups so nothing is lost.

use std::collections::BTreeMap;

/// A parsed desktop file.
///
/// Groups and keys are stored verbatim, including localized keys such as
/// `Name[es]` and vendor extensions such as `X-AppImage-Version`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopFile {
    sections: BTreeMap<String, BTreeMap<String, String>>,
}

impl DesktopFile {
    /// Parses the given desktop file contents.
    ///
    /// Unknown constructs are ignored rather than rejected, mirroring what the
    /// original Python implementation accepts in the wild.
    #[must_use]
    pub fn parse(contents: &str) -> Self {
        let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let mut current: Option<String> = None;

        for line in contents.lines() {
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(group) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let group = group.trim().to_owned();
                sections.entry(group.clone()).or_default();
                current = Some(group);
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };

            if let Some(section) = current.as_deref() {
                sections
                    .entry(section.to_owned())
                    .or_default()
                    .insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }

        Self { sections }
    }

    /// Returns the keys of `section`, if it exists.
    #[must_use]
    pub fn section(&self, section: &str) -> Option<&BTreeMap<String, String>> {
        self.sections.get(section)
    }

    /// Returns one value from `section`.
    ///
    /// Lookup is exact, so localized keys must be requested by their full key
    /// (for example `Name[es]`).
    #[must_use]
    pub fn value(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(section)
            .and_then(|keys| keys.get(key))
            .map(String::as_str)
    }

    /// Iterates over the group names.
    pub fn sections(&self) -> impl Iterator<Item = &str> {
        self.sections.keys().map(String::as_str)
    }

    /// Returns the value of the `[Desktop Entry]` group, or an empty map.
    #[must_use]
    pub fn main_section(&self) -> BTreeMap<String, String> {
        self.sections
            .get("Desktop Entry")
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys_and_groups() {
        let contents = "\
# comment
[Desktop Entry]
Name=Example
Exec=/opt/example %U
Terminal=false
X-AppImage-Version=1.2.3

[Desktop Action new-window]
Name=New Window
Exec=/opt/example --new-window
";
        let file = DesktopFile::parse(contents);

        assert_eq!(file.value("Desktop Entry", "Name"), Some("Example"));
        assert_eq!(file.value("Desktop Entry", "Exec"), Some("/opt/example %U"));
        assert_eq!(
            file.value("Desktop Action new-window", "Exec"),
            Some("/opt/example --new-window")
        );
        assert_eq!(file.sections().count(), 2);
    }

    #[test]
    fn ignores_comments_and_lines_without_section() {
        let contents = "Name=Orphan\n\n[Desktop Entry]\n#Name=Ignored\nName=Kept\n";
        let file = DesktopFile::parse(contents);

        assert_eq!(file.value("Desktop Entry", "Name"), Some("Kept"));
        assert_eq!(file.value("Desktop Entry", "Orphan"), None);
    }

    #[test]
    fn last_value_wins_and_crlf_is_accepted() {
        let contents = "[Desktop Entry]\r\nName=First\r\nName=Second\r\n";
        let file = DesktopFile::parse(contents);

        assert_eq!(file.value("Desktop Entry", "Name"), Some("Second"));
    }

    #[test]
    fn localized_keys_are_preserved() {
        let contents = "[Desktop Entry]\nName=Gear\nName[es]=Palanca\n";

        assert_eq!(
            DesktopFile::parse(contents).value("Desktop Entry", "Name[es]"),
            Some("Palanca")
        );
    }
}
