//! Host capabilities that decide whether AppImages can be mounted.
//!
//! Gear LeverR uses a static AppImage runtime that needs `/dev/fuse` and a
//! `fusermount` helper to mount the image. The runtime used by the official
//! builds embeds FUSE 3, so the host does not need `libfuse3`. Older AppImages
//! built with the classic runtime still need `libfuse2`, which is reported
//! separately from the mounting capability.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use std::os::unix::fs::{FileTypeExt, PermissionsExt};

/// Host capabilities related to FUSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each field is an independent probe of the host, not a flag set"
)]
pub struct FuseReport {
    device: bool,
    fusermount: bool,
    fusermount3: bool,
    libfuse2: bool,
}

impl FuseReport {
    /// Inspects the host for the pieces AppImage runtimes use to mount images.
    #[must_use]
    pub fn detect() -> Self {
        let device = Path::new("/dev/fuse")
            .metadata()
            .is_ok_and(|metadata| metadata.file_type().is_char_device());

        Self {
            device,
            fusermount: find_helper("fusermount"),
            fusermount3: find_helper("fusermount3"),
            libfuse2: libfuse2_available(),
        }
    }

    /// Returns whether the `/dev/fuse` device exists.
    #[must_use]
    pub const fn has_device(self) -> bool {
        self.device
    }

    /// Returns whether the FUSE 2 `fusermount` helper is in `PATH`.
    #[must_use]
    pub const fn has_fuse2_helper(self) -> bool {
        self.fusermount
    }

    /// Returns whether the FUSE 3 `fusermount3` helper is in `PATH`.
    #[must_use]
    pub const fn has_fuse3_helper(self) -> bool {
        self.fusermount3
    }

    /// Returns whether `libfuse.so.2` can be loaded.
    ///
    /// This is only needed by AppImages built with the classic runtime. Gear
    /// LeverR and AppImages built with the static runtime do not use it.
    #[must_use]
    pub const fn has_libfuse2(self) -> bool {
        self.libfuse2
    }

    /// Returns whether a FUSE mount can be attempted.
    #[must_use]
    pub const fn can_mount(self) -> bool {
        self.device && (self.fusermount || self.fusermount3)
    }

    /// Returns whether older AppImages can mount with `libfuse2`.
    #[must_use]
    pub const fn legacy_ready(self) -> bool {
        self.device && self.fusermount && self.libfuse2
    }

    /// Returns whether both FUSE 2 and FUSE 3 are ready for AppImages.
    ///
    /// Gear LeverR itself can run with either generation, but an AppImage
    /// manager should not install until both the legacy and the current helper
    /// are available.
    #[must_use]
    pub const fn all_ready(self) -> bool {
        self.can_mount() && self.fusermount3 && self.legacy_ready()
    }

    /// Returns the most important problem found on the host, if any.
    ///
    /// A missing device takes precedence over missing helpers, then over the
    /// FUSE 2 compatibility layer, and finally over the optional FUSE 3 helper.
    #[must_use]
    pub const fn problem(self) -> Option<FuseProblem> {
        if !self.device {
            Some(FuseProblem::MissingDevice)
        } else if !self.fusermount && !self.fusermount3 {
            Some(FuseProblem::MissingTools)
        } else if !self.legacy_ready() {
            Some(FuseProblem::MissingFuse2)
        } else if !self.fusermount3 {
            Some(FuseProblem::MissingFuse3)
        } else {
            None
        }
    }
}

/// A host problem that prevents AppImages from mounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FuseProblem {
    /// The `/dev/fuse` device is not present.
    MissingDevice,
    /// Neither `fusermount` nor `fusermount3` is available.
    MissingTools,
    /// The FUSE 2 compatibility layer is incomplete.
    ///
    /// Only AppImages built with the classic runtime need it.
    MissingFuse2,
    /// FUSE 2 is ready, but the FUSE 3 helper is missing.
    ///
    /// Gear LeverR can still mount with FUSE 2, but newer AppImages may need
    /// the FUSE 3 helper.
    MissingFuse3,
}

/// The package set needed to fix a FUSE-related problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FuseComponent {
    /// Packages needed to mount images, which is FUSE 3 on supported systems.
    Mount,
    /// Packages needed by AppImages built with the classic runtime.
    Legacy,
    /// Both the mounting and the legacy packages.
    All,
}

impl FuseComponent {
    /// Returns the distribution package hint for this component.
    ///
    /// Returns `None` when the distribution is unknown or when its package
    /// names change with the release in a way this build cannot determine.
    /// Callers should show a link to the AppImage documentation in that case.
    #[must_use]
    pub fn install_command(self) -> Option<String> {
        let release = OsRelease::load()?;
        install_command_for(self, &release)
    }
}

/// How the current process was started from an AppImage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunningMode {
    /// The AppImage filesystem is mounted through FUSE.
    Mounted,
    /// The AppImage was extracted to a temporary directory.
    Extracted,
    /// The process was not started from an AppImage.
    NotAnAppImage,
    /// `APPDIR` is set but the mount table could not be checked.
    Unknown,
}

/// Returns how the current process was started from an AppImage.
#[must_use]
pub fn running_mode() -> RunningMode {
    let Some(appdir) = env::var_os("APPDIR").map(PathBuf::from) else {
        return RunningMode::NotAnAppImage;
    };

    if appdir.as_os_str().is_empty() {
        return RunningMode::NotAnAppImage;
    }

    match fs::read_to_string("/proc/self/mountinfo") {
        Ok(mountinfo) => running_mode_from_mountinfo(&appdir, &mountinfo),
        Err(_) => RunningMode::Unknown,
    }
}

fn running_mode_from_mountinfo(appdir: &Path, mountinfo: &str) -> RunningMode {
    let Ok(appdir) = fs::canonicalize(appdir) else {
        return RunningMode::Unknown;
    };

    if mount_points(mountinfo).any(|mount_point| mount_point == appdir) {
        RunningMode::Mounted
    } else {
        RunningMode::Extracted
    }
}

fn mount_points(mountinfo: &str) -> impl Iterator<Item = PathBuf> + '_ {
    mountinfo.lines().filter_map(|line| {
        line.split(' ')
            .nth(4)
            .map(|mount_point| PathBuf::from(unescape_mountinfo(mount_point)))
    })
}

fn unescape_mountinfo(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn find_helper(name: &str) -> bool {
    let mut directories: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();

    for fallback in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let directory = PathBuf::from(fallback);
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }

    find_in_directories(name, &directories)
}

fn find_in_directories(name: &str, directories: &[PathBuf]) -> bool {
    directories
        .iter()
        .any(|directory| is_executable(&directory.join(name)))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn libfuse2_available() -> bool {
    // SAFETY: the C string is static and NUL-terminated. `dlopen` only maps a
    // shared object and does not call into it.
    let handle =
        unsafe { libc::dlopen(c"libfuse.so.2".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return false;
    }

    // SAFETY: `handle` came from a successful `dlopen` and has not been closed.
    unsafe {
        let _ = libc::dlclose(handle);
    }

    true
}

#[derive(Debug, Default)]
struct OsRelease {
    id: String,
    id_like: Vec<String>,
    version_id: Option<String>,
}

impl OsRelease {
    fn load() -> Option<Self> {
        let text = fs::read_to_string("/etc/os-release").ok()?;
        Some(parse_os_release(&text))
    }

    fn is_family(&self, family: &str) -> bool {
        self.id == family || self.id_like.iter().any(|like| like == family)
    }
}

fn parse_os_release(text: &str) -> OsRelease {
    let mut release = OsRelease::default();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = trim_quotes(value.trim());

        match key {
            "ID" => value.clone_into(&mut release.id),
            "ID_LIKE" => {
                release.id_like = value.split_whitespace().map(ToOwned::to_owned).collect();
            }
            "VERSION_ID" => release.version_id = Some(value.to_owned()),
            _ => {}
        }
    }

    release
}

fn trim_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }

    value
}

fn version_tuple(release: &OsRelease) -> Option<(u32, u32)> {
    let version = release.version_id.as_deref()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);

    Some((major, minor))
}

fn debian_legacy_package(release: &OsRelease) -> Option<&'static str> {
    let version = version_tuple(release)?;

    let t64 = match release.id.as_str() {
        "debian" => version >= (13, 0),
        "ubuntu" => version >= (24, 4),
        "linuxmint" => version >= (22, 0),
        _ => return None,
    };

    Some(if t64 { "libfuse2t64" } else { "libfuse2" })
}

fn install_command_for(component: FuseComponent, release: &OsRelease) -> Option<String> {
    if release.is_family("debian") || release.is_family("ubuntu") || release.id == "linuxmint" {
        let mount = "fuse3";
        let legacy = debian_legacy_package(release)?;

        return Some(match component {
            FuseComponent::Mount => format!("sudo apt install {mount}"),
            FuseComponent::Legacy => format!("sudo apt install {legacy}"),
            FuseComponent::All => format!("sudo apt install {mount} {legacy}"),
        });
    }

    if release.is_family("fedora") || release.is_family("rhel") {
        return Some(match component {
            FuseComponent::Mount => "sudo dnf install fuse3".to_owned(),
            FuseComponent::Legacy => "sudo dnf install fuse".to_owned(),
            FuseComponent::All => "sudo dnf install fuse3 fuse".to_owned(),
        });
    }

    if release.is_family("arch") {
        return Some(match component {
            FuseComponent::Mount => "sudo pacman -S fuse3".to_owned(),
            FuseComponent::Legacy => "sudo pacman -S fuse2".to_owned(),
            FuseComponent::All => "sudo pacman -S fuse3 fuse2".to_owned(),
        });
    }

    if release.is_family("suse") {
        return Some(match component {
            FuseComponent::Mount => "sudo zypper install fuse3".to_owned(),
            FuseComponent::Legacy => "sudo zypper install fuse".to_owned(),
            FuseComponent::All => "sudo zypper install fuse3 fuse".to_owned(),
        });
    }

    if release.is_family("alpine") {
        return Some(match component {
            FuseComponent::Mount => "sudo apk add fuse3".to_owned(),
            FuseComponent::Legacy => "sudo apk add fuse".to_owned(),
            FuseComponent::All => "sudo apk add fuse3 fuse".to_owned(),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the helper mirrors the independent probes of the report"
    )]
    fn report(device: bool, fusermount: bool, fusermount3: bool, libfuse2: bool) -> FuseReport {
        FuseReport {
            device,
            fusermount,
            fusermount3,
            libfuse2,
        }
    }

    fn release(text: &str) -> OsRelease {
        parse_os_release(text)
    }

    #[test]
    fn missing_device_takes_precedence() {
        assert_eq!(
            report(false, true, true, true).problem(),
            Some(FuseProblem::MissingDevice)
        );
    }

    #[test]
    fn missing_helpers_need_fuse_three() {
        assert_eq!(
            report(true, false, false, true).problem(),
            Some(FuseProblem::MissingTools)
        );
        assert_eq!(
            report(true, false, true, true).problem(),
            Some(FuseProblem::MissingFuse2)
        );
        assert_eq!(
            report(true, true, false, false).problem(),
            Some(FuseProblem::MissingFuse2)
        );
    }

    #[test]
    fn all_ready_requires_both_fuse_generations() {
        assert!(report(true, true, true, true).all_ready());
        assert!(!report(true, true, false, true).all_ready());
        assert!(!report(true, false, true, true).all_ready());
        assert!(!report(false, true, true, true).all_ready());
        assert!(!report(true, true, true, false).all_ready());
    }

    #[test]
    fn missing_fuse_three_with_fuse_two_ready() {
        let report = report(true, true, false, true);

        assert!(report.can_mount());
        assert!(report.legacy_ready());
        assert_eq!(report.problem(), Some(FuseProblem::MissingFuse3));
    }

    #[test]
    fn reports_a_healthy_host() {
        let report = report(true, true, true, true);
        assert!(report.can_mount());
        assert!(report.legacy_ready());
        assert_eq!(report.problem(), None);
    }

    #[test]
    fn fuse_three_helper_is_enough_to_mount() {
        let report = report(true, false, true, false);
        assert!(report.can_mount());
        assert!(!report.legacy_ready());
        assert_eq!(report.problem(), Some(FuseProblem::MissingFuse2));
    }

    #[test]
    fn finds_executables_in_directories() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let helper = directory.path().join("fusermount3");

        assert!(!find_in_directories(
            "fusermount3",
            &[directory.path().to_path_buf()]
        ));

        fs::write(&helper, b"#!/bin/sh\n").expect("write helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).expect("make executable");

        assert!(find_in_directories(
            "fusermount3",
            &[directory.path().to_path_buf()]
        ));
    }

    #[test]
    fn ignores_unreadable_helpers() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let helper = directory.path().join("fusermount");

        fs::write(&helper, b"not executable").expect("write helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o644)).expect("write mode");

        assert!(!find_in_directories(
            "fusermount",
            &[directory.path().to_path_buf()]
        ));
    }

    #[test]
    fn parses_os_release_values() {
        let release = release("ID=ubuntu\nID_LIKE=\"debian\"\nVERSION_ID=\"24.04\"\n# comment\n");

        assert_eq!(release.id, "ubuntu");
        assert_eq!(release.id_like, ["debian"]);
        assert_eq!(release.version_id.as_deref(), Some("24.04"));
    }

    #[test]
    fn selects_debian_and_ubuntu_package_names() {
        let debian12 = release("ID=debian\nVERSION_ID=\"12\"\n");
        let debian13 = release("ID=debian\nVERSION_ID=\"13\"\n");
        let ubuntu2204 = release("ID=ubuntu\nVERSION_ID=\"22.04\"\n");
        let ubuntu2404 = release("ID=ubuntu\nVERSION_ID=\"24.04\"\n");

        assert_eq!(
            install_command_for(FuseComponent::All, &debian12).as_deref(),
            Some("sudo apt install fuse3 libfuse2")
        );
        assert_eq!(
            install_command_for(FuseComponent::All, &debian13).as_deref(),
            Some("sudo apt install fuse3 libfuse2t64")
        );
        assert_eq!(
            install_command_for(FuseComponent::Legacy, &ubuntu2204).as_deref(),
            Some("sudo apt install libfuse2")
        );
        assert_eq!(
            install_command_for(FuseComponent::Mount, &ubuntu2404).as_deref(),
            Some("sudo apt install fuse3")
        );
    }

    #[test]
    fn selects_mint_package_names() {
        let mint21 = release("ID=linuxmint\nVERSION_ID=\"21.3\"\n");
        let mint22 = release("ID=linuxmint\nVERSION_ID=\"22\"\n");

        assert_eq!(
            install_command_for(FuseComponent::Legacy, &mint21).as_deref(),
            Some("sudo apt install libfuse2")
        );
        assert_eq!(
            install_command_for(FuseComponent::Legacy, &mint22).as_deref(),
            Some("sudo apt install libfuse2t64")
        );
    }

    #[test]
    fn selects_other_distribution_commands() {
        let fedora = release("ID=fedora\nVERSION_ID=40\n");
        let arch = release("ID=arch\n");
        let suse = release("ID=opensuse-tumbleweed\nID_LIKE=\"opensuse suse\"\n");
        let alpine = release("ID=alpine\nVERSION_ID=3.20\n");

        assert_eq!(
            install_command_for(FuseComponent::All, &fedora).as_deref(),
            Some("sudo dnf install fuse3 fuse")
        );
        assert_eq!(
            install_command_for(FuseComponent::All, &arch).as_deref(),
            Some("sudo pacman -S fuse3 fuse2")
        );
        assert_eq!(
            install_command_for(FuseComponent::All, &suse).as_deref(),
            Some("sudo zypper install fuse3 fuse")
        );
        assert_eq!(
            install_command_for(FuseComponent::All, &alpine).as_deref(),
            Some("sudo apk add fuse3 fuse")
        );
    }

    #[test]
    fn refuses_to_guess_unknown_distributions() {
        let unknown = release("ID=plan9\nVERSION_ID=1\n");

        assert_eq!(install_command_for(FuseComponent::All, &unknown), None);
    }

    #[test]
    fn refuses_to_guess_missing_versions() {
        let debian = release("ID=debian\n");

        assert_eq!(install_command_for(FuseComponent::All, &debian), None);
    }

    #[test]
    fn refuses_to_guess_unparsable_versions() {
        let debian = release("ID=debian\nVERSION_ID=testing\n");

        assert_eq!(install_command_for(FuseComponent::All, &debian), None);
    }

    #[test]
    fn unescapes_mountinfo_paths() {
        assert_eq!(
            unescape_mountinfo("/tmp/with\\040space\\134name"),
            "/tmp/with space\\name"
        );
    }

    #[test]
    fn detects_mounted_and_extracted_appdirs() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let appdir = fs::canonicalize(directory.path()).expect("canonical path");
        let mountinfo = format!(
            "25 1 0:22 / {} rw,nosuid - tmpfs tmpfs rw\n",
            appdir.display()
        );

        assert_eq!(
            running_mode_from_mountinfo(&appdir, &mountinfo),
            RunningMode::Mounted
        );
        assert_eq!(
            running_mode_from_mountinfo(&appdir, ""),
            RunningMode::Extracted
        );
    }
}
