//! Shell-style parsing and quoting of desktop entry `Exec` lines.
//!
//! The original Gear Lever uses Python's [`shlex`] for this and writes `Exec`
//! values with `shlex.join`. Staying byte-compatible keeps already integrated
//! applications working and makes the CLI tests meaningful.
//!
//! [`shlex`]: https://docs.python.org/3/library/shlex.html

use std::fmt::Write as _;

use crate::error::{Error, Result};

/// Characters that need no quoting, matching `shlex.quote`.
const SAFE: &[u8] = b"_@%+=:,./-";

/// A desktop entry `Exec` line split into its parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandLine {
    /// `KEY=value` prefixes such as `DESKTOPINTEGRATION=1`.
    pub env_variables: Vec<String>,
    /// First token that is neither an environment variable nor a flag.
    pub executable: String,
    /// Remaining tokens, including flags.
    pub arguments: Vec<String>,
}

impl CommandLine {
    /// Joins the arguments with single spaces, as the original does when it
    /// stores them in the configuration file.
    #[must_use]
    pub fn arguments_joined(&self) -> String {
        self.arguments.join(" ")
    }
}

/// Splits an `Exec` line the way `extract_terminal_arguments` does in the
/// original implementation.
///
/// # Errors
///
/// Returns [`Error::InvalidCommandLine`] when the quotes are unbalanced.
pub fn parse(command: &str) -> Result<CommandLine> {
    let tokens =
        shlex::split(command).ok_or_else(|| Error::InvalidCommandLine(command.to_owned()))?;
    let mut parsed = CommandLine::default();

    for token in tokens {
        if token == "env" {
            continue;
        }

        let looks_like_env =
            token.contains('=') && !token.starts_with('/') && !token.starts_with('-');

        if looks_like_env {
            parsed.env_variables.push(token);
        } else if parsed.executable.is_empty() && !token.starts_with('-') {
            parsed.executable = token;
        } else {
            parsed.arguments.push(token);
        }
    }

    Ok(parsed)
}

/// Splits an `Exec` line into plain arguments.
///
/// # Errors
///
/// Returns [`Error::InvalidCommandLine`] when the quotes are unbalanced.
pub fn split(command: &str) -> Result<Vec<String>> {
    shlex::split(command).ok_or_else(|| Error::InvalidCommandLine(command.to_owned()))
}

/// Quotes one argument like `shlex.quote`.
#[must_use]
pub fn quote(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || SAFE.contains(&byte))
    {
        return argument.to_owned();
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('\'');
    for character in argument.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

/// Quotes and joins arguments like `shlex.join`.
#[must_use]
pub fn join<'a>(arguments: impl IntoIterator<Item = &'a str>) -> String {
    let mut joined = String::new();

    for (index, argument) in arguments.into_iter().enumerate() {
        if index > 0 {
            joined.push(' ');
        }
        let _ = write!(joined, "{}", quote(argument));
    }

    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_env_executable_and_arguments() {
        let parsed =
            parse("env DESKTOPINTEGRATION=1 A=2 '/opt/My App.AppImage' --flag %U").unwrap();

        assert_eq!(parsed.env_variables, vec!["DESKTOPINTEGRATION=1", "A=2"]);
        assert_eq!(parsed.executable, "/opt/My App.AppImage");
        assert_eq!(parsed.arguments, vec!["--flag", "%U"]);
    }

    #[test]
    fn keeps_flags_that_appear_before_the_executable() {
        let parsed = parse("env -i /opt/app --keep").unwrap();

        assert_eq!(parsed.executable, "/opt/app");
        assert_eq!(parsed.arguments, vec!["-i", "--keep"]);
    }

    #[test]
    fn rejects_unbalanced_quotes() {
        assert!(matches!(
            parse("'/opt/app"),
            Err(Error::InvalidCommandLine(_))
        ));
    }

    #[test]
    fn quotes_like_python_shlex() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("/opt/app.AppImage"), "/opt/app.AppImage");
        assert_eq!(quote("with space"), "'with space'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("%U"), "%U");
    }

    #[test]
    fn joins_and_splits_round_trip() {
        let arguments = ["/opt/My App.AppImage", "--title", "a b"];
        let joined = join(arguments);

        assert_eq!(joined, "'/opt/My App.AppImage' --title 'a b'");
        assert_eq!(split(&joined).unwrap(), arguments);
    }
}
