//! Prints a few translated labels from a compiled catalog.
//!
//! ```sh
//! scripts/compile-translations.sh
//! cargo run -p gearlever --example i18n -- target es_ES.UTF-8
//! ```

#![expect(clippy::print_stdout, reason = "this example's output is its result")]

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let directory = env::args().nth(1).unwrap_or_else(|| "target".to_owned());
    let language = env::args().nth(2).unwrap_or_else(|| "es".to_owned());

    // SAFETY: the example is single-threaded and this runs before any output.
    let _ = unsafe {
        gettextrs::TextDomain::new("gearlever-r")
            .locale(&language)
            .push(directory)
            .codeset("UTF-8")
            .init()
    };

    for message in [
        "Installed applications",
        "Advanced",
        "Close tutorial",
        "Update",
        "No updates available",
    ] {
        println!("{message} -> {}", gettextrs::gettext(message));
    }

    ExitCode::SUCCESS
}
