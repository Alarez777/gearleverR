//! Parsing of zsync control files.
//!
//! AppImage developers publish a `.zsync` file next to every release. Its header
//! carries the SHA-1 of the file and, sometimes, a relative or absolute URL to
//! the actual AppImage. Gear LeverR uses the header to decide whether an update
//! exists and keeps the raw bytes for a delta download.

use url::Url;

/// Keep a malformed control file from turning the header scan into a full pass
/// over the block checksums. Real headers are a few hundred bytes.
const MAX_HEADER_BYTES: usize = 1 << 20;

/// The parts of a zsync header that Gear LeverR uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZsyncInfo {
    /// SHA-1 of the target file, lower-case hex.
    pub sha1: Option<String>,
    /// `URL:` entry, which may be relative to the `.zsync` URL.
    pub url: Option<String>,
    /// `Length:` of the target file, when the publisher includes it.
    pub length: Option<u64>,
}

/// Parses the header of a zsync control file.
#[must_use]
pub fn parse(contents: &str) -> ZsyncInfo {
    parse_bytes(contents.as_bytes())
}

/// Parses the header from the raw control file, including its binary tail.
///
/// Only the lines before the first empty line are inspected, so the block
/// checksums after the header are never decoded as text.
#[must_use]
pub fn parse_bytes(contents: &[u8]) -> ZsyncInfo {
    let mut info = ZsyncInfo::default();
    let mut consumed = 0_usize;

    for line in contents.split(|byte| *byte == b'\n') {
        consumed = consumed.saturating_add(line.len().saturating_add(1));
        if consumed > MAX_HEADER_BYTES {
            break;
        }

        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }

        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };

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
        } else if let Some(value) = line.strip_prefix("Length:") {
            info.length = value.trim().parse().ok();
        }
    }

    info
}

/// Returns the directory of `zsync_url`, against which relative `URL:` entries
/// in the control file are resolved.
#[must_use]
pub(crate) fn base_url(zsync_url: &str) -> String {
    let without_query = zsync_url.split(['?', '#']).next().unwrap_or(zsync_url);

    without_query
        .rfind('/')
        .map(|index| without_query[..=index].to_owned())
        .unwrap_or_default()
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
Length: 123456
\n\nbinary data";
        let info = parse(contents);

        assert_eq!(
            info.sha1.as_deref(),
            Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")
        );
        assert_eq!(info.url.as_deref(), Some("app.AppImage"));
        assert_eq!(info.length, Some(123_456));
    }

    #[test]
    fn parses_the_header_without_decoding_the_binary_tail() {
        let mut contents =
            b"zsync: 0.6.2\nSHA-1: AAF4C61DDCC5E8A2DABEDE0F3B482CD9AEA9434D\nLength: 8\n\n"
                .to_vec();
        contents.extend([0xFF, 0xFE, 0x00, 0x80]);

        let info = parse_bytes(&contents);

        assert_eq!(
            info.sha1.as_deref(),
            Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")
        );
        assert_eq!(info.length, Some(8));
    }

    #[test]
    fn stops_reading_after_the_header_limit() {
        let mut contents = b"SHA-1: AAF4C61DDCC5E8A2DABEDE0F3B482CD9AEA9434D\n".to_vec();
        while contents.len() <= MAX_HEADER_BYTES {
            contents.extend_from_slice(b"X-Filler: value\n");
        }
        contents.extend_from_slice(b"Length: 42\n\n");

        let info = parse_bytes(&contents);

        assert_eq!(
            info.sha1.as_deref(),
            Some("aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d")
        );
        assert_eq!(info.length, None, "fields after the cap are ignored");
    }

    #[test]
    fn derives_the_base_url_from_the_control_location() {
        assert_eq!(
            base_url("https://example.com/releases/app.AppImage.zsync"),
            "https://example.com/releases/"
        );
        assert_eq!(
            base_url("https://example.com/app.AppImage.zsync?token=1#frag"),
            "https://example.com/"
        );
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
