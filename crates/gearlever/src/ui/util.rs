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
pub const DETAILS_ICON: i32 = 112;

/// Lower-cases and removes spaces, matching the search in the original.
#[must_use]
pub fn normalize(value: &str) -> String {
    value.to_lowercase().replace(' ', "")
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
    label.add_css_class("pill");
    label
}
