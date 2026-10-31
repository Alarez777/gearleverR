//! Reader and writer for freedesktop.org desktop entries.
//!
//! Gear LeverR needs to read entries embedded in AppImages and to rewrite the
//! integrated copy while preserving unknown keys and `[Desktop Action ...]`
//! groups. The file is kept as a map of groups so nothing is lost.

use std::collections::BTreeMap;
use std::fmt;

/// A parsed desktop file.
///
/// Groups and keys are stored verbatim, including localized keys such as
/// `Name[es]` and vendor extensions such as `X-AppImage-Version`. Writing is
/// deterministic: `[Desktop Entry]` comes first and the rest is sorted.
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
                    .insert(key.trim().to_owned(), unescape(value.trim()));
            }
        }

        Self { sections }
    }

    /// Returns the keys of `section`, if it exists.
    #[must_use]
    pub fn section(&self, section: &str) -> Option<&BTreeMap<String, String>> {
        self.sections.get(section)
    }

    /// Returns a mutable view of `section`, creating it when missing.
    pub fn section_mut(&mut self, section: &str) -> &mut BTreeMap<String, String> {
        self.sections.entry(section.to_owned()).or_default()
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

    /// Sets one value, creating the section when needed.
    pub fn set(&mut self, section: &str, key: &str, value: impl Into<String>) {
        self.section_mut(section)
            .insert(key.to_owned(), value.into());
    }

    /// Removes one key, if present.
    pub fn remove(&mut self, section: &str, key: &str) {
        if let Some(keys) = self.sections.get_mut(section) {
            keys.remove(key);
        }
    }

    /// Removes a whole section.
    pub fn remove_section(&mut self, section: &str) {
        self.sections.remove(section);
    }

    /// Iterates over the group names.
    pub fn sections(&self) -> impl Iterator<Item = &str> {
        self.sections.keys().map(String::as_str)
    }

    /// Returns the `[Desktop Entry]` group, or an empty map.
    #[must_use]
    pub fn main_section(&self) -> BTreeMap<String, String> {
        self.section("Desktop Entry").cloned().unwrap_or_default()
    }

    /// Returns the action section names, in the order they are written.
    #[must_use]
    pub fn action_sections(&self) -> Vec<&str> {
        self.sections
            .keys()
            .filter(|name| name.starts_with("Desktop Action "))
            .map(String::as_str)
            .collect()
    }
}

impl fmt::Display for DesktopFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;

        if let Some(values) = self.sections.get("Desktop Entry") {
            write_section(formatter, "Desktop Entry", values, &mut first)?;
        }

        for (name, values) in &self.sections {
            if name == "Desktop Entry" {
                continue;
            }
            write_section(formatter, name, values, &mut first)?;
        }

        Ok(())
    }
}

fn write_section(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    values: &BTreeMap<String, String>,
    first: &mut bool,
) -> fmt::Result {
    if !*first {
        formatter.write_str("\n")?;
    }
    writeln!(formatter, "[{name}]")?;

    for (key, value) in values {
        writeln!(formatter, "{key}={}", escape(value))?;
    }

    *first = false;
    Ok(())
}

/// Escapes a value as required by the desktop entry specification.
fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for (index, character) in value.chars().enumerate() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ' ' if index == 0 => escaped.push_str("\\s"),
            other => escaped.push(other),
        }
    }

    escaped
}

/// Decodes the escape sequences allowed in desktop entry values.
fn unescape(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars();

    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }

        match characters.next() {
            Some('s') => decoded.push(' '),
            Some('n') => decoded.push('\n'),
            Some('t') => decoded.push('\t'),
            Some('r') => decoded.push('\r'),
            Some('\\') | None => decoded.push('\\'),
            Some(other) => {
                decoded.push('\\');
                decoded.push(other);
            }
        }
    }

    decoded
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
        assert_eq!(file.action_sections(), vec!["Desktop Action new-window"]);
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

    #[test]
    fn escapes_and_unescapes_values() {
        let contents = "[Desktop Entry]\nComment=line1\\nline2\nName=\\sIndented\n";

        let file = DesktopFile::parse(contents);
        assert_eq!(file.value("Desktop Entry", "Comment"), Some("line1\nline2"));
        assert_eq!(file.value("Desktop Entry", "Name"), Some(" Indented"));

        let rendered = file.to_string();
        assert!(rendered.contains("Comment=line1\\nline2"));
        assert!(rendered.contains("Name=\\sIndented"));
    }

    #[test]
    fn writes_desktop_entry_first_and_actions_after() {
        let mut file = DesktopFile::parse(
            "[Desktop Action z]\nName=Z\n\n[Desktop Entry]\nName=Example\n\n[Desktop Action a]\nName=A\n",
        );
        file.set("Desktop Entry", "Exec", "/opt/app");

        let rendered = file.to_string();
        let desktop = rendered.find("[Desktop Entry]").unwrap();
        let action_a = rendered.find("[Desktop Action a]").unwrap();
        let action_z = rendered.find("[Desktop Action z]").unwrap();

        assert!(desktop < action_a);
        assert!(action_a < action_z);
    }

    #[test]
    fn mutating_and_removing_keys() {
        let mut file = DesktopFile::parse("[Desktop Entry]\nName=One\n");
        file.set("Desktop Entry", "Name", "Two");
        file.set("Desktop Action x", "Name", "X");
        file.remove("Desktop Entry", "Name");
        file.remove_section("Desktop Action x");

        assert_eq!(file.value("Desktop Entry", "Name"), None);
        assert!(file.action_sections().is_empty());
    }
}
