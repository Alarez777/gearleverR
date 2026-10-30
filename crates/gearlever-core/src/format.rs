//! Formatting helpers shared by the CLI and the interface.

/// Formats a byte count with GNU-style binary suffixes.
///
/// ```text
/// 900     -> "900 B"
/// 1536    -> "1.5 KB"
/// 87206392 -> "83.2 MB"
/// ```
#[must_use]
pub fn size(bytes: u64) -> String {
    const SUFFIXES: [&str; 9] = ["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "displaying a size does not need exact precision"
    )]
    let mut value = bytes as f64;
    let mut index = 0;

    while value >= 1024.0 && index < SUFFIXES.len() - 1 {
        value /= 1024.0;
        index += 1;
    }

    format!("{value:.1} {}", SUFFIXES[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_binary_sizes() {
        assert_eq!(size(900), "900 B");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(87_206_392), "83.2 MB");
        assert_eq!(size(1024 * 1024 * 1024), "1.0 GB");
    }
}
