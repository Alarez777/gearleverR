//! Gear Lever R: a GTK4 front-end and CLI for managing AppImages.

#![expect(
    clippy::print_stderr,
    reason = "main reports command failures on standard error"
)]

mod cli;
mod i18n;
mod settings;
mod ui;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::Parser;
use gearlever_core::paths::Paths;
use tracing_subscriber::fmt::MakeWriter;

use crate::settings::Settings;

fn main() -> ExitCode {
    init_logging();

    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let has_option = arguments
        .iter()
        .any(|argument| argument.to_string_lossy().starts_with('-'));

    if !has_option {
        let files: Vec<PathBuf> = arguments
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .collect();

        ui::run(files);
        return ExitCode::SUCCESS;
    }

    let cli = cli::Cli::parse();

    if !cli.has_command() {
        ui::run(Vec::new());
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
    let paths = Paths::from_env().ok();
    let debug = paths
        .as_ref()
        .is_some_and(|paths| Settings::load(paths).values().debug_logs);

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        if debug {
            "debug".to_owned()
        } else {
            "warn".to_owned()
        }
    });

    let writer = paths
        .and_then(|paths| log_file(&paths).map(TeeWriter::new))
        .unwrap_or_else(TeeWriter::stderr_only);

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .try_init();
}

fn log_file(paths: &Paths) -> Option<fs::File> {
    let path = paths.log_file();
    fs::create_dir_all(path.parent()?).ok()?;

    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 2 * 1024 * 1024) {
        let _ = fs::File::create(&path);
    }

    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// Writes logs to standard error and, when possible, to a file.
#[derive(Debug, Clone)]
struct TeeWriter {
    file: Option<Arc<Mutex<fs::File>>>,
}

impl TeeWriter {
    fn new(file: fs::File) -> Self {
        Self {
            file: Some(Arc::new(Mutex::new(file))),
        }
    }

    fn stderr_only() -> Self {
        Self { file: None }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buffer);

        if let Some(file) = &self.file {
            file.lock()
                .expect("log file lock is never held across a panic")
                .write_all(buffer)?;
        }

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(file) = &self.file {
            file.lock()
                .expect("log file lock is never held across a panic")
                .flush()?;
        }
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for TeeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
