//! Translation support built on gettext.
//!
//! The catalogs live in `po/` and are compiled to `.mo` files under
//! `share/locale/<lang>/LC_MESSAGES/gearlever-r.mo`. In an AppImage, `AppRun`
//! points `GEARLEVER_R_LOCALEDIR` at the bundled directory.

use std::path::PathBuf;

/// Initializes gettext for the interface.
///
/// Called once, before any widget is created.
pub(crate) fn init() {
    let directory = locale_prefix();

    // SAFETY: this runs at startup, before any thread is spawned, and the
    // directory is a process-wide gettext binding.
    let _ = unsafe {
        gettextrs::TextDomain::new("gearlever-r")
            .locale_category(gettextrs::LocaleCategory::LcAll)
            .push(directory)
            .codeset("UTF-8")
            .init()
    };
}

/// Translates one message.
#[must_use]
pub(crate) fn t(text: &str) -> String {
    gettextrs::gettext(text)
}

/// Translates a message with `{name}` style placeholders.
#[must_use]
pub(crate) fn tf(template: &str, values: &[(&str, &str)]) -> String {
    let mut text = t(template);
    for (key, value) in values {
        text = text.replace(&format!("{{{key}}}"), value);
    }
    text
}

/// Returns the prefix that contains the `locale` directory.
///
/// gettext looks for `<prefix>/locale/<lang>/LC_MESSAGES/<domain>.mo`, so the
/// AppImage exports `$APPDIR/usr/share` and a development build passes `target`
/// (because translations are compiled to `target/locale`).
fn locale_prefix() -> PathBuf {
    if let Some(directory) = std::env::var_os("GEARLEVER_R_LOCALEDIR") {
        return directory.into();
    }

    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        let candidate = parent.join("usr/share");
        if candidate.join("locale").is_dir() {
            return candidate;
        }
    }

    PathBuf::from("/usr/share")
}
