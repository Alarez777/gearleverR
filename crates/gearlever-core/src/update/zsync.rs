//! Parsing of zsync control files.
//!
//! AppImage developers publish a `.zsync` file next to every release. Its header
//! carries the SHA-1 of the file and, sometimes, a relative or absolute URL to
//! the actual AppImage. Gear LeverR does a full download, but it uses the
//! header to decide whether an update exists.

use url::Url;

/// The parts of a zsync header that Gear LeverR uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZsyncInfo {
    /// SHA-1 of the target file, lower-case hex.
    pub sha1: Option<String>,
    /// `URL:` entry, which may be relative to the `.zsync` URL.
    pub url: Option<String>,
}

/// Parses the header of a zsync control file.
#[must_use]
pub fn parse(contents: &str) -> ZsyncInfo {
    let header = contents.split("\n\n").next().unwrap_or_default();
    let mut info = ZsyncInfo::default();

    for line in header.lines() {
        if let Some(value) = line.strip_prefix("SHA-1:") {
            let value = value.trim().to_ascii_lowercase();
            if !value.is_empty() {
                info.sha1 = Some(value);
            }
        } else if let Some(value) = line.strip_prefix("URL:") {
            let value = value.trim();
            if !value.is_empty() {
                info.url = Some(value.to_owned());
            }
        }
    }

    info
}

/// Resolves the download URL from a zsync header.
///
/// Relative URLs are resolved against the `.zsync` location. When the header has
/// no `URL:` entry, the `.zsync` suffix is removed from `zsync_url`.
#[must_use]
pub fn download_url(zsync_url: &str, info: &ZsyncInfo) -> String {
    let Some(candidate) = info.url.as_deref() else {
        return zsync_url
            .strip_suffix(".zsync")
            .unwrap_or(zsync_url)
            .to_owned();
    };

    if let Ok(absolute) = Url::parse(candidate) {
        return absolute.into();
    }

    Url::parse(zsync_url)
        .and_then(|base| base.join(candidate))
        .map_or_else(|_| candidate.to_owned(), Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_header_only() {
        let contents = "\
zsync: 0.6.2
Filename: app.AppImage
SHA-1: AAF4C61DDCC5E8A2DABEDE0F3B482CD9AEA9434D
URL: app.AppImage
\n\nbinary data";
        let info = parse(contents);

        assert_eq!(
            info.sha1.as_deref(),
            Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")
        );
        assert_eq!(info.url.as_deref(), Some("app.AppImage"));
    }

    #[test]
    fn resolves_relative_and_absolute_urls() {
        let relative = ZsyncInfo {
            url: Some("app.AppImage".to_owned()),
            ..ZsyncInfo::default()
        };
        assert_eq!(
            download_url("https://example.com/releases/app.AppImage.zsync", &relative),
            "https://example.com/releases/app.AppImage"
        );

        let absolute = ZsyncInfo {
            url: Some("https://cdn.example.com/app.AppImage".to_owned()),
            ..ZsyncInfo::default()
        };
        assert_eq!(
            download_url("https://example.com/app.AppImage.zsync", &absolute),
            "https://cdn.example.com/app.AppImage"
        );
    }

    #[test]
    fn strips_the_zsync_suffix_when_there_is_no_url() {
        let info = ZsyncInfo {
            sha1: Some("abc".to_owned()),
            ..ZsyncInfo::default()
        };

        assert_eq!(
            download_url("https://example.com/app.AppImage.zsync", &info),
            "https://example.com/app.AppImage"
        );
    }
}
