//! Small UI helpers: icons, text normalisation and labels.

#![expect(unreachable_pub, reason = "private module of the binary")]

use std::fs;
use std::path::Path;

use relm4::adw::prelude::*;
use relm4::gtk;
use relm4::gtk::{gdk, glib};

use gearlever_core::appimage::AppImage;
use gearlever_core::cache::MetadataCache;
use gearlever_core::scan::InstalledApp;

/// Icon size used in list rows.
pub const ROW_ICON: i32 = 40;
/// Icon size used on the details page.
pub const DETAILS_ICON: i32 = 128;

/// Lower-cases and removes spaces, matching the search in the original.
#[must_use]
pub fn normalize(value: &str) -> String {
    value.to_lowercase().replace(' ', "")
}

/// Replaces the home directory prefix with `~` for display.
#[must_use]
pub fn home_relative(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Builds the icon of an integrated application.
///
/// The order matches the original: the desktop entry path, then the themed icon
/// name, then the icon embedded in the AppImage (cached on disk).
#[must_use]
pub fn app_image(app: &InstalledApp, cache_dir: &Path, pixels: i32) -> gtk::Image {
    if let Some(path) = &app.icon_path
        && let Some(texture) = texture_from_file(path)
    {
        return image_from_texture(&texture, pixels);
    }

    if let Some(name) = &app.icon
        && has_themed_icon(name)
    {
        let image = gtk::Image::from_icon_name(name);
        image.set_pixel_size(pixels);
        return image;
    }

    if let Some(texture) = embedded_icon(app, cache_dir) {
        return image_from_texture(&texture, pixels);
    }

    let image = gtk::Image::from_icon_name("application-x-executable-symbolic");
    image.set_pixel_size(pixels);
    image
}

/// Builds an icon from raw bytes, such as the icon of a file that is not
/// integrated yet.
#[must_use]
pub fn image_from_bytes(bytes: &[u8], pixels: i32) -> gtk::Image {
    if let Some(texture) = texture_from_bytes(bytes) {
        image_from_texture(&texture, pixels)
    } else {
        let image = gtk::Image::from_icon_name("application-x-executable-symbolic");
        image.set_pixel_size(pixels);
        image
    }
}

fn image_from_texture(texture: &gdk::Texture, pixels: i32) -> gtk::Image {
    let image = gtk::Image::from_paintable(Some(texture));
    image.set_pixel_size(pixels);
    image
}

fn texture_from_file(path: &Path) -> Option<gdk::Texture> {
    let bytes = fs::read(path).ok()?;
    texture_from_bytes(&bytes)
}

fn texture_from_bytes(bytes: &[u8]) -> Option<gdk::Texture> {
    let bytes = glib::Bytes::from(bytes);
    gdk::Texture::from_bytes(&bytes).ok()
}

fn has_themed_icon(name: &str) -> bool {
    gdk::Display::default()
        .is_some_and(|display| gtk::IconTheme::for_display(&display).has_icon(name))
}

fn embedded_icon(app: &InstalledApp, cache_dir: &Path) -> Option<gdk::Texture> {
    let cache = MetadataCache::new(cache_dir);

    if let Ok(Some(cached)) = cache.load(&app.image_path)
        && let Some(path) = cache.icon_path(&app.image_path, &cached)
        && let Ok(bytes) = fs::read(path)
        && let Some(texture) = texture_from_bytes(&bytes)
    {
        return Some(texture);
    }

    let metadata = AppImage::open(&app.image_path).ok()?.metadata().ok()?;
    let icon = metadata.icon.as_ref()?;
    let texture = texture_from_bytes(&icon.bytes)?;
    let _ = cache.store(&app.image_path, &metadata);

    Some(texture)
}

/// Returns the path of an icon for a file that is not integrated yet.
#[must_use]
pub fn candidate_icon(metadata: &gearlever_core::appimage::AppMetadata, pixels: i32) -> gtk::Image {
    metadata.icon.as_ref().map_or_else(
        || {
            let image = gtk::Image::from_icon_name("application-x-executable-symbolic");
            image.set_pixel_size(pixels);
            image
        },
        |icon| image_from_bytes(&icon.bytes, pixels),
    )
}

/// Returns a label styled as a pill.
#[must_use]
pub fn pill(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("app-pill");
    label
}

/// Static styles used by the interface.
const STYLE: &str = r"
    .app-pill {
        background-color: alpha(@theme_fg_color, 0.1);
        border-radius: 9999px;
        padding: 4px 12px;
    }
    .arch-warning {
        background-color: alpha(@warning_color, 0.15);
        border-radius: 12px;
        padding: 12px;
    }
    .multi-progress {
        margin: 0;
        padding: 0;
    }
    .multi-progress > trough {
        min-height: 4px;
        border-radius: 0;
    }
    .multi-progress > trough > progress {
        min-height: 4px;
        border-radius: 0;
    }
    .update-progress {
        margin-top: 2px;
    }
    .update-progress > trough {
        min-height: 4px;
        border-radius: 9999px;
    }
    .update-progress > trough > progress {
        min-height: 4px;
        border-radius: 9999px;
        background-color: @accent_bg_color;
    }
    .update-progress.success > trough > progress {
        background-color: @success_bg_color;
    }
";

/// Installs the small stylesheet used by the interface.
///
/// libadwaita 1.4 has `AdwToggleGroup`, but the project targets 1.2, so the
/// details page builds its pills from labels and styles them here. The window
/// font size applies the saved interface scale to every label and button.
pub fn install_css(ui_scale: u32) {
    let Some(display) = gdk::Display::default() else {
        return;
    };

    let em = f64::from(ui_scale.max(100)) / 100.0;
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&format!("window {{ font-size: {em:.2}em; }}\n{STYLE}"));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::home_relative;

    #[test]
    fn abbreviates_paths_inside_home() {
        assert_eq!(
            home_relative(
                Path::new("/home/alarez/AppImages"),
                Path::new("/home/alarez")
            ),
            "~/AppImages"
        );
        assert_eq!(
            home_relative(Path::new("/home/alarez"), Path::new("/home/alarez")),
            "~"
        );
    }

    #[test]
    fn keeps_paths_outside_home_untouched() {
        assert_eq!(
            home_relative(Path::new("/opt/AppImages"), Path::new("/home/alarez")),
            "/opt/AppImages"
        );
    }
}
