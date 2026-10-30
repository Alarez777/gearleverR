//! Lists the paths inside an AppImage's SquashFS payload.
//!
//! ```sh
//! cargo run -p gearlever-core --example entries -- ~/AppImages/app.appimage
//! ```

#![expect(clippy::print_stdout, reason = "this example's output is its result")]
#![expect(clippy::print_stderr, reason = "this example's output is its result")]

use std::env;
use std::path::Path;
use std::process::ExitCode;

use gearlever_core::appimage::{AppImage, SquashfsImage};

fn main() -> ExitCode {
    let Some(path) = env::args_os().nth(1) else {
        eprintln!("usage: entries <path/to/file.AppImage>");
        return ExitCode::FAILURE;
    };

    match run(Path::new(&path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &Path) -> gearlever_core::Result<()> {
    let appimage = AppImage::open(path)?;
    let image = SquashfsImage::open(path, appimage.payload_offset())?;

    for entry in image.paths() {
        println!("{}", entry.display());
    }

    Ok(())
}
