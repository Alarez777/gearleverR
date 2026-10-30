//! Gear Lever R: a GTK4 front-end and CLI for managing AppImages.

#![expect(
    clippy::print_stderr,
    reason = "main reports command failures on standard error"
)]

mod cli;
mod settings;
mod ui;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    init_logging();

    let cli = cli::Cli::parse();

    if !cli.has_command() {
        ui::run();
        return ExitCode::SUCCESS;
    }

    match cli.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_owned());

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
