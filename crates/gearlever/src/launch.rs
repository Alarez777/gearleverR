//! Environment handling for the processes Gear LeverR starts.
//!
//! When Gear LeverR runs as an AppImage, its `AppRun` exports the paths of the
//! bundled libraries, schemas, icons and GTK modules. A child AppImage has to
//! start with its own values: inheriting ours mixes two GTK stacks, which
//! breaks the theme, the icons and the desktop settings of the launched app.

use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Starts `program` once the current process exits.
///
/// Gear LeverR owns a unique application ID on the session bus, so a second
/// instance started while this one runs only forwards an activation request and
/// quits. The helper waits for this process to disappear and then starts the
/// installed AppImage, which keeps a single instance and lets the AppImage
/// runtime unmount the current image before the next one starts.
///
/// # Errors
///
/// Returns [`std::io::Error`] when the helper process cannot be spawned or the
/// AppImage has disappeared since the list was scanned.
pub(crate) fn relaunch_after_exit(program: &Path) -> std::io::Result<()> {
    relaunch_after(std::process::id(), program, None)
}

/// Starts `program` with `file` as its only argument once the current process
/// exits.
///
/// The candidate AppImage opens that file after the restart, so the new
/// version keeps showing the install page.
///
/// # Errors
///
/// Returns [`std::io::Error`] when the helper process cannot be spawned or the
/// AppImage has disappeared since the list was scanned.
pub(crate) fn relaunch_after_exit_with_file(program: &Path, file: &Path) -> std::io::Result<()> {
    relaunch_after(std::process::id(), program, Some(file))
}

/// Starts `program` once `pid` is no longer running.
fn relaunch_after(pid: u32, program: &Path, file: Option<&Path>) -> std::io::Result<()> {
    // `/proc/<pid>/exe` stops resolving as soon as the process exits, even
    // while a zombie remains, so the replacement does not have to wait for the
    // parent to reap it. `$#` is 2 without a file and 3 with one.
    const WAIT: &str = "while [ -e \"/proc/$1/exe\" ]; do sleep 0.05; done; \
                        if [ \"$#\" -ge 3 ]; then exec \"$2\" \"$3\"; else exec \"$2\"; fi";

    if !program.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is not a file", program.display()),
        ));
    }

    let mut helper = command("/bin/sh");

    // `command` strips the AppImage runtime variables so child apps start
    // clean. This helper starts the same AppImage again, so it must keep the
    // extraction mode used to launch the current instance; otherwise a restart
    // fails on systems without FUSE.
    if let Some(value) = std::env::var_os("APPIMAGE_EXTRACT_AND_RUN") {
        helper.env("APPIMAGE_EXTRACT_AND_RUN", value);
    }

    helper
        .arg("-c")
        .arg(WAIT)
        .arg("sh")
        .arg(pid.to_string())
        .arg(program);

    if let Some(file) = file {
        helper.arg(file);
    }

    // The replacement keeps stdout and stderr, so a startup failure still
    // reaches the journal or the terminal that started Gear LeverR.
    helper
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_child| ())
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
    use std::os::unix::fs::PermissionsExt;

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

    fn fake_program(directory: &Path, marker: &Path) -> PathBuf {
        let program = directory.join("fake-appimage");
        std::fs::write(&program, format!("#!/bin/sh\n: > '{}'\n", marker.display()))
            .expect("write the fake program");

        let mut permissions = std::fs::metadata(&program)
            .expect("read the fake program permissions")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).expect("make the fake program executable");

        program
    }

    fn wait_for_marker(marker: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

        while !marker.is_file() {
            assert!(
                std::time::Instant::now() < deadline,
                "the replacement did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn rejects_a_missing_program() {
        let error = relaunch_after(999_999_999, Path::new("/missing/app.AppImage"), None)
            .expect_err("the missing program is rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn starts_the_replacement_when_the_process_is_gone() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("started");
        let program = fake_program(directory.path(), &marker);

        // Above the default and maximum pid_max on Linux.
        relaunch_after(999_999_999, &program, None).expect("spawn the replacement");

        wait_for_marker(&marker);
    }

    #[test]
    fn waits_until_the_process_exits() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("started");
        let program = fake_program(directory.path(), &marker);

        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 0.3")
            .spawn()
            .expect("spawn the process to wait for");
        let pid = child.id();

        relaunch_after(pid, &program, None).expect("spawn the replacement");

        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !marker.exists(),
            "the replacement started while the process was still running"
        );

        child.wait().expect("wait for the process");
        wait_for_marker(&marker);
    }

    fn fake_program_with_argument(directory: &Path, marker: &Path) -> PathBuf {
        let program = directory.join("fake-appimage-with-file");
        std::fs::write(
            &program,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", marker.display()),
        )
        .expect("write the fake program");

        let mut permissions = std::fs::metadata(&program)
            .expect("read the fake program permissions")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).expect("make the fake program executable");

        program
    }

    #[test]
    fn passes_the_file_to_the_replacement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = directory.path().join("argument");
        let program = fake_program_with_argument(directory.path(), &marker);
        let file = directory.path().join("candidate.AppImage");
        std::fs::write(&file, b"candidate").expect("write the candidate");

        relaunch_after(999_999_999, &program, Some(&file)).expect("spawn the replacement");

        wait_for_marker(&marker);
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read the argument"),
            file.to_string_lossy().as_ref()
        );
    }
}
