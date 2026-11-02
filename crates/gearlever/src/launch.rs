//! Environment handling for the processes Gear LeverR starts.
//!
//! When Gear LeverR runs as an AppImage, its `AppRun` exports the paths of the
//! bundled libraries, schemas, icons and GTK modules. A child AppImage has to
//! start with its own values: inheriting ours mixes two GTK stacks, which
//! breaks the theme, the icons and the desktop settings of the launched app.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Variables that the AppImage runtime and the linuxdeploy hooks add.
const APPIMAGE_VARS: &[&str] = &[
    "APPDIR",
    "APPIMAGE",
    "ARGV0",
    "OWD",
    "APPIMAGE_EXTRACT_AND_RUN",
    "GTK_DATA_PREFIX",
    "GTK_EXE_PREFIX",
    "GTK_PATH",
    "GDK_PIXBUF_MODULE_FILE",
    "GI_TYPELIB_PATH",
    "GEARLEVER_R_LOCALEDIR",
];

/// Path lists that may still point inside our own AppDir.
const PATH_VARS: &[&str] = &["LD_LIBRARY_PATH", "XDG_DATA_DIRS", "GSETTINGS_SCHEMA_DIR"];

/// Builds a command for a process Gear LeverR starts.
///
/// The AppImage variables are dropped and the entries that point inside our own
/// AppDir are removed from the path lists, so the child keeps the values the
/// user set and none of ours.
#[must_use]
pub(crate) fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    let environment: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);

    sanitize(&mut command, &environment, appdir.as_deref());

    command
}

/// Drops our AppImage environment from the command.
fn sanitize(command: &mut Command, environment: &[(OsString, OsString)], appdir: Option<&Path>) {
    for name in APPIMAGE_VARS {
        command.env_remove(name);
    }

    let Some(appdir) = appdir else {
        return;
    };

    for name in PATH_VARS {
        let Some((_, value)) = environment.iter().find(|(key, _)| key == name) else {
            continue;
        };

        match without_dir(value, appdir) {
            Some(value) => {
                command.env(name, value);
            }
            None => {
                command.env_remove(name);
            }
        }
    }
}

/// Drops the entries of a path list that live under `appdir`.
fn without_dir(value: &OsStr, appdir: &Path) -> Option<OsString> {
    let kept: Vec<PathBuf> = std::env::split_paths(value)
        .filter(|entry| !entry.starts_with(appdir))
        .collect();

    std::env::join_paths(kept)
        .ok()
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    #[test]
    fn drops_the_appimage_variables() {
        let environment = environment(&[
            ("APPDIR", "/tmp/.mount_GearAbc"),
            ("APPIMAGE", "/home/tester/AppImages/GearLeverR.AppImage"),
            ("GTK_DATA_PREFIX", "/tmp/.mount_GearAbc"),
            ("LD_LIBRARY_PATH", "/tmp/.mount_GearAbc/usr/lib:/usr/lib"),
        ]);
        let mut command = Command::new("true");

        sanitize(
            &mut command,
            &environment,
            Some(Path::new("/tmp/.mount_GearAbc")),
        );

        let vars: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();

        assert!(vars.contains(&(OsStr::new("APPDIR"), None)));
        assert!(vars.contains(&(OsStr::new("APPIMAGE"), None)));
        assert!(vars.contains(&(OsStr::new("GTK_DATA_PREFIX"), None)));
        assert!(vars.contains(&(OsStr::new("LD_LIBRARY_PATH"), Some(OsStr::new("/usr/lib")))));
    }

    #[test]
    fn drops_a_path_variable_that_only_points_inside_the_appdir() {
        let environment = environment(&[("XDG_DATA_DIRS", "/tmp/.mount_GearAbc/usr/share")]);
        let mut command = Command::new("true");

        sanitize(
            &mut command,
            &environment,
            Some(Path::new("/tmp/.mount_GearAbc")),
        );

        let vars: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();

        assert!(vars.contains(&(OsStr::new("XDG_DATA_DIRS"), None)));
    }

    #[test]
    fn keeps_the_user_environment_without_an_appdir() {
        let environment = environment(&[("LD_LIBRARY_PATH", "/opt/lib")]);
        let mut command = Command::new("true");

        sanitize(&mut command, &environment, None);

        let vars: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();

        assert!(
            !vars
                .iter()
                .any(|(key, _)| *key == OsStr::new("LD_LIBRARY_PATH"))
        );
    }

    #[test]
    fn drops_the_entries_that_point_inside_the_appdir() {
        let appdir = Path::new("/tmp/.mount_GearAbc");
        let value = OsStr::new("/tmp/.mount_GearAbc/usr/lib:/usr/lib:/opt/lib");

        assert_eq!(
            without_dir(value, appdir),
            Some(OsString::from("/usr/lib:/opt/lib"))
        );
    }

    #[test]
    fn keeps_a_sibling_directory_with_the_same_prefix() {
        let appdir = Path::new("/tmp/.mount_GearAbc");
        let value = OsStr::new("/tmp/.mount_GearAbc2/usr/lib");

        assert_eq!(
            without_dir(value, appdir),
            Some(OsString::from("/tmp/.mount_GearAbc2/usr/lib"))
        );
    }
}
