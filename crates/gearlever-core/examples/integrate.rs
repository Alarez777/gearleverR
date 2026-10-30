//! Integrates one AppImage into a throwaway home directory and prints the
//! generated desktop entry. Useful to compare the output with the original.
//!
//! ```sh
//! cargo run -p gearlever-core --example integrate -- \
//!     ~/Downloads/app.appimage /tmp/glr-home
//! ```

#![expect(clippy::print_stdout, reason = "this example's output is its result")]
#![expect(clippy::print_stderr, reason = "this example's output is its result")]

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use gearlever_core::appimage::AppImage;
use gearlever_core::config::Config;
use gearlever_core::install::{IntegrateOptions, integrate};
use gearlever_core::paths::Paths;

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);
    let (Some(source), Some(home)) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: integrate <file.AppImage> <home-directory>");
        return ExitCode::FAILURE;
    };

    match run(Path::new(&source), Path::new(&home)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(source: &Path, home: &Path) -> gearlever_core::Result<()> {
    let paths = Paths::with_home(home);
    let metadata = AppImage::open(source)?.metadata()?;

    let mut options =
        IntegrateOptions::new(paths.default_appimages_dir(), paths.applications_dir());
    options.move_source = false;
    options.update_desktop_database = false;

    let mut config = Config::default();
    let app = integrate(source, &metadata, None, &options, &mut config)?;
    config.save(&paths.config_file())?;

    println!("image:   {}", app.image_path.display());
    println!("desktop: {}", app.desktop_path.display());
    println!("icon:    {}", app.icon.as_deref().unwrap_or("-"));
    println!("config:  {}", paths.config_file().display());
    println!("\n{}", fs::read_to_string(&app.desktop_path)?);
    println!("{}", fs::read_to_string(paths.config_file())?);

    Ok(())
}
