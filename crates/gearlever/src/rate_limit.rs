//! User-facing messages for GitHub API rate limits.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gearlever_core::error::RateLimitInfo;

use crate::i18n::{t, tf};

/// What the check knew about the token when GitHub reported the limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenState {
    /// A token was used for the request.
    Used,
    /// A token is configured but could not be loaded, for example because the
    /// system keyring is locked.
    ConfiguredNotLoaded,
    /// No token is configured.
    None,
}

/// Builds the short message shown in a toast or desktop notification.
pub(crate) fn message(info: &RateLimitInfo, token: TokenState) -> String {
    let wait = wait(info).map(format_wait);

    match (token, wait) {
        (TokenState::Used, Some(wait)) => tf("Rate limit: retry in {wait}.", &[("wait", &wait)]),
        (TokenState::Used, None) => t("Rate limit exhausted. Retry later."),
        (TokenState::ConfiguredNotLoaded, Some(wait)) => tf(
            "Rate limit: unlock the keyring or retry in {wait}.",
            &[("wait", &wait)],
        ),
        (TokenState::ConfiguredNotLoaded, None) => {
            t("Rate limit: unlock the keyring or add a token.")
        }
        (TokenState::None, Some(wait)) => tf(
            "Rate limit: retry in {wait} or add a token.",
            &[("wait", &wait)],
        ),
        (TokenState::None, None) => t("Rate limit exhausted. Add a token in Preferences."),
    }
}

fn wait(info: &RateLimitInfo) -> Option<Duration> {
    if let Some(seconds) = info.retry_after {
        return Some(Duration::from_secs(seconds));
    }

    let reset = info.reset?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    reset.checked_sub(now).map(Duration::from_secs)
}

fn format_wait(wait: Duration) -> String {
    let seconds = wait.as_secs();
    if seconds < 60 {
        let seconds = seconds.to_string();
        tf("{seconds} s", &[("seconds", &seconds)])
    } else {
        let minutes = seconds.div_ceil(60).to_string();
        tf("{minutes} min", &[("minutes", &minutes)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(retry_after: Option<u64>, reset: Option<u64>) -> RateLimitInfo {
        RateLimitInfo { reset, retry_after }
    }

    #[test]
    fn formats_short_and_long_waits() {
        assert_eq!(format_wait(Duration::from_secs(30)), "30 s");
        assert_eq!(format_wait(Duration::from_secs(61)), "2 min");
        assert_eq!(format_wait(Duration::from_secs(3600)), "60 min");
    }

    #[test]
    fn uses_retry_after_before_reset() {
        assert_eq!(
            wait(&info(Some(45), Some(10))),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn includes_the_wait_and_token_state() {
        let text = message(&info(Some(90), None), TokenState::None);
        assert!(text.contains("2 min"), "{text}");
        assert!(text.contains("add a token"), "{text}");

        let text = message(&info(Some(30), None), TokenState::ConfiguredNotLoaded);
        assert!(text.contains("unlock the keyring"), "{text}");
    }
}
