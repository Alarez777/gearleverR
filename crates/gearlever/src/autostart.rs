//! Registers the background update check for the next login.
//!
//! The XDG Background portal is asked first, because it respects the desktop's
//! own autostart settings and works from a sandbox. If the portal is not
//! available, Gear Lever R falls back to a classic `~/.config/autostart` entry.

use std::fs;
use std::path::{Path, PathBuf};

use gearlever_core::args;
use gearlever_core::desktop::DesktopFile;
use gearlever_core::paths::Paths;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::glib::prelude::ToVariant;

/// Enables or disables the login-time `--fetch-updates` check.
///
/// # Errors
///
/// Returns a human-readable message when neither the portal nor the fallback
/// entry can be written.
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the setting is a boolean switch"
)]
pub(crate) fn apply(paths: &Paths, enabled: bool) -> Result<(), String> {
    let command = commandline()?;

    if request_background(enabled, &command).is_ok() {
        remove(&paths.autostart_file());
        return Ok(());
    }

    tracing::debug!("background portal unavailable, using the autostart file");

    if enabled {
        write_entry(&paths.autostart_file(), &command)
            .map_err(|error| format!("cannot write the autostart entry: {error}"))?;
    } else {
        remove(&paths.autostart_file());
    }

    Ok(())
}

fn commandline() -> Result<Vec<String>, String> {
    let executable = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "cannot determine the executable path".to_owned())?;

    Ok(vec![
        executable.to_string_lossy().into_owned(),
        "--fetch-updates".to_owned(),
    ])
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the portal option is a boolean switch"
)]
fn request_background(enabled: bool, command: &[String]) -> Result<(), String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|error| error.to_string())?;

    let options = glib::VariantDict::new(None);
    options.insert_value(
        "reason",
        &"Gear Lever R background update checks".to_variant(),
    );
    options.insert_value("autostart", &enabled.to_variant());
    options.insert_value("background", &true.to_variant());
    options.insert_value("commandline", &command.to_variant());
    let options = options.end();

    let parameters = glib::Variant::tuple_from_iter(["".to_variant(), options]);

    connection
        .call_sync(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Background",
            "RequestBackground",
            Some(&parameters),
            None,
            gio::DBusCallFlags::NONE,
            1500,
            gio::Cancellable::NONE,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn write_entry(path: &Path, command: &[String]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let exec = command
        .iter()
        .map(|argument| args::quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
        .replace('%', "%%");

    let mut desktop = DesktopFile::default();
    desktop.set("Desktop Entry", "Type", "Application");
    desktop.set("Desktop Entry", "Name", "Gear Lever R updates");
    desktop.set(
        "Desktop Entry",
        "Comment",
        "Check for AppImage updates at login",
    );
    desktop.set("Desktop Entry", "Exec", exec);
    desktop.set("Desktop Entry", "Terminal", "false");
    desktop.set("Desktop Entry", "X-GNOME-Autostart-enabled", "true");

    fs::write(path, desktop.to_string())
}

fn remove(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "cannot remove autostart"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_quoted_fallback_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("autostart/entry.desktop");
        let command = vec![
            "/tmp/My App.AppImage".to_owned(),
            "--fetch-updates".to_owned(),
        ];

        write_entry(&path, &command).unwrap();

        let contents = fs::read_to_string(path).unwrap();
        let desktop = DesktopFile::parse(&contents);
        assert_eq!(
            desktop.value("Desktop Entry", "Exec"),
            Some("'/tmp/My App.AppImage' --fetch-updates")
        );
        assert_eq!(
            desktop.value("Desktop Entry", "X-GNOME-Autostart-enabled"),
            Some("true")
        );
    }

    #[test]
    fn commandline_is_a_string_array() {
        let command = vec!["/opt/app.AppImage".to_owned(), "--fetch-updates".to_owned()];

        assert_eq!(command.to_variant().type_().as_str(), "as");
    }
}
