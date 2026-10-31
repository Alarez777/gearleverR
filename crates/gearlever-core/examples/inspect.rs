//! Prints the metadata Gear LeverR can read from one AppImage.
//!
//! ```sh
//! cargo run -p gearlever-core --example inspect -- ~/AppImages/app.appimage
//! ```

#![expect(clippy::print_stdout, reason = "this example's output is its result")]
#![expect(clippy::print_stderr, reason = "this example's output is its result")]

use std::env;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use gearlever_core::Error;
use gearlever_core::appimage::AppImage;

fn main() -> ExitCode {
    let Some(path) = env::args_os().nth(1) else {
        eprintln!("usage: inspect <path/to/file.AppImage>");
        return ExitCode::FAILURE;
    };

    let started = Instant::now();

    match inspect(Path::new(&path)) {
        Ok(()) => {
            println!("elapsed:   {:?}", started.elapsed());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn inspect(path: &Path) -> gearlever_core::Result<()> {
    let appimage = AppImage::open(path)?;

    println!("file:      {}", path.display());
    println!("type:      {}", appimage.image_type());
    println!(
        "payload:   {} at offset {}",
        appimage.payload_kind(),
        appimage.payload_offset()
    );
    println!("arch:      {}", appimage.arch());

    match appimage.metadata() {
        Ok(metadata) => {
            println!(
                "desktop:   {}",
                metadata.desktop_file.as_deref().unwrap_or("-")
            );
            println!("name:      {}", metadata.name);
            println!("version:   {}", metadata.version.as_deref().unwrap_or("-"));
            println!("terminal:  {}", metadata.terminal);
            println!("exec:      {}", metadata.exec);
            println!("comment:   {}", metadata.comment);

            match metadata.icon {
                Some(icon) => {
                    println!("icon:      {} ({} bytes)", icon.file_name, icon.bytes.len());
                }
                None => println!("icon:      -"),
            }
        }
        Err(Error::UnsupportedPayload(_)) => {
            println!("metadata:  payload format not supported yet");
        }
        Err(error) => return Err(error),
    }

    Ok(())
}
