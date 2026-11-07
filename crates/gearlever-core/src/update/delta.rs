//! Delta downloads built on the `zsync-rs` library.
//!
//! A source that publishes a `.zsync` control file describes the new release
//! as a list of block checksums. Scanning the installed AppImage lets zsync
//! copy the blocks that did not change and request only the rest with HTTP
//! range requests. Any failure falls back to the full download handled by the
//! calling manager; cancellation is the only error that must be propagated.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::time::{Duration, Instant};

use zsync_rs::{AssemblyError, ZsyncAssembly};

use super::DownloadContext;
use super::http::HttpClient;
use crate::error::{Error, Result};

/// Maximum retries after a transient range-request failure.
const MAX_RETRIES: u32 = 3;

/// How often a worker waiting for the scan lock checks for cancellation.
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(test)]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Only one AppImage is scanned at a time: the matcher already uses every
/// available core.
static SCAN_LOCK: Mutex<()> = Mutex::new(());

/// Downloads `zsync_url` into the staging directory, using `seed` as the
/// installed AppImage.
///
/// Progress is reported on the calling thread. The transfer itself runs on a
/// worker because the `zsync-rs` callbacks must be `'static`, while
/// [`DownloadContext::progress`] borrows the caller's closure.
///
/// # Errors
///
/// Returns [`Error::Cancelled`] when the caller cancels, and [`Error::Http`] or
/// [`Error::Io`] for every other failure. Managers are expected to fall back to
/// a full download for any error except [`Error::Cancelled`].
pub(crate) fn download(
    zsync_url: &str,
    seed: &Path,
    context: &mut DownloadContext<'_>,
    http: &HttpClient,
) -> Result<PathBuf> {
    if context.is_cancelled() {
        return Err(Error::Cancelled);
    }

    let metadata =
        fs::metadata(seed).map_err(|error| Error::Http(format!("{}: {error}", seed.display())))?;
    if !metadata.is_file() {
        return Err(Error::Http(format!(
            "{}: zsync seed is not a regular file",
            seed.display()
        )));
    }

    let output = context.output_path();
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }

    let (sender, receiver) = mpsc::sync_channel::<f64>(1);
    let progress = Arc::new(Mutex::new(sender));
    let cancel = Arc::clone(&context.cancel);
    let zsync_url = zsync_url.to_owned();
    let seed = seed.to_path_buf();
    let output_for_worker = output.clone();
    let agent = http.zsync_agent();

    let worker = std::thread::Builder::new()
        .name("zsync-delta".to_owned())
        .spawn(move || {
            run(
                &zsync_url,
                &seed,
                &output_for_worker,
                agent,
                &cancel,
                progress,
            )
        })
        .map_err(|error| Error::Http(format!("cannot start zsync worker: {error}")))?;

    let mut last = 0.0_f64;
    while let Ok(fraction) = receiver.recv() {
        let fraction = fraction.clamp(0.0, 1.0);
        if fraction > last {
            last = fraction;
            (context.progress)(fraction);
        }
    }

    worker
        .join()
        .map_err(|_| Error::Http("zsync worker panicked".to_owned()))??;

    if last < 1.0 {
        (context.progress)(1.0);
    }

    Ok(output)
}

fn run(
    zsync_url: &str,
    seed: &Path,
    output: &Path,
    agent: ureq::Agent,
    cancel: &Arc<AtomicBool>,
    progress: Arc<Mutex<mpsc::SyncSender<f64>>>,
) -> Result<()> {
    let zsync_http = zsync_rs::HttpClient::with_agent(agent);
    let mut assembly = ZsyncAssembly::from_url_with_client(zsync_url, output, zsync_http)
        .map_err(|error| map_assembly_error(error, cancel))?;
    assembly.set_abort_flag(Arc::clone(cancel));

    let progress_for_callback = Arc::clone(&progress);
    assembly.set_progress_callback(move |done, total| {
        if total == 0 {
            return;
        }
        let fraction = fraction(done, total);
        if let Ok(sender) = progress_for_callback.lock() {
            let _ = sender.send(fraction);
        }
    });
    // The callback now owns the only sender. Dropping this copy lets the
    // receiving loop finish as soon as the assembly is gone.
    drop(progress);

    {
        let _guard = lock_for_scan(cancel)?;
        let found = assembly
            .submit_source_file(seed)
            .map_err(|error| map_assembly_error(error, cancel))?;
        let _ = assembly
            .submit_self_referential()
            .map_err(|error| map_assembly_error(error, cancel))?;
        tracing::debug!(blocks = found, "zsync scan finished");
    }

    let (known, total) = assembly.block_stats();
    if known == 0 {
        return Err(Error::Http("zsync found no matching blocks".to_owned()));
    }
    tracing::debug!(known, total, "zsync blocks available before download");

    let mut retries = 0_u32;
    loop {
        match assembly.download_missing_blocks() {
            Ok(0) => break,
            Ok(_) => retries = 0,
            Err(error) if error.is_transient() && retries < MAX_RETRIES => {
                retries += 1;
                tracing::debug!(
                    attempt = retries,
                    %error,
                    "zsync range download failed, retrying"
                );
                sleep_or_cancel(retry_delay(retries), cancel)?;
            }
            Err(error) => return Err(map_assembly_error(error, cancel)),
        }
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    assembly
        .complete()
        .map_err(|error| map_assembly_error(error, cancel))?;
    Ok(())
}

fn lock_for_scan(cancel: &AtomicBool) -> Result<std::sync::MutexGuard<'static, ()>> {
    loop {
        match SCAN_LOCK.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => {
                return Err(Error::Http("zsync scan lock poisoned".to_owned()));
            }
            Err(TryLockError::WouldBlock) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                std::thread::sleep(SCAN_POLL_INTERVAL);
            }
        }
    }
}

fn retry_delay(retries: u32) -> Duration {
    RETRY_BASE_DELAY.saturating_mul(1_u32 << retries)
}

fn sleep_or_cancel(delay: Duration, cancel: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(SCAN_POLL_INTERVAL));
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn map_assembly_error(error: AssemblyError, cancel: &AtomicBool) -> Error {
    match error {
        AssemblyError::Aborted if cancel.load(Ordering::Relaxed) => Error::Cancelled,
        other => Error::Http(format!("zsync: {other}")),
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "download progress does not need exact precision"
)]
fn fraction(done: u64, total: u64) -> f64 {
    done as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use zsync_rs::ControlFile;

    use super::*;
    use crate::update::DownloadContext;
    use crate::update::HttpClient;
    use crate::update::test_fixtures::{
        BLOCK_SIZE, FILE_SIZE, changed_copy, control_bytes, delta_server, pattern,
    };
    use crate::update::test_server::Route;

    fn context<'a>(
        directory: &Path,
        progress: &'a mut dyn FnMut(f64),
        cancel: Arc<AtomicBool>,
    ) -> DownloadContext<'a> {
        let mut context =
            DownloadContext::with_cancel(directory.join("downloads"), progress, cancel);
        context.use_zsync_delta = true;
        context
    }

    #[test]
    fn reconstructs_the_target_from_a_seed() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new.clone()));
        let mut progress = Vec::new();
        let cancel = Arc::new(AtomicBool::new(false));

        let result = {
            let mut progress_callback = |fraction| progress.push(fraction);
            let mut context = context(directory.path(), &mut progress_callback, cancel);
            download(
                &server.url("/new.AppImage.zsync"),
                &seed,
                &mut context,
                &HttpClient::new(),
            )
        }
        .unwrap();

        assert_eq!(fs::read(&result).unwrap(), new);
        assert!(
            server.range_requests() > 0,
            "the delta path must use range requests"
        );
        assert!(
            server.range_bytes() < u64::try_from(new.len()).unwrap(),
            "the delta path must download fewer bytes than the full file"
        );
        assert!(
            progress.windows(2).all(|pair| pair[0] <= pair[1]),
            "progress must be monotonic: {progress:?}"
        );
        assert_eq!(progress.last().copied(), Some(1.0));
        assert!(
            !has_zsync_temp(directory.path()),
            "the temporary zsync file must be removed"
        );
    }

    #[test]
    fn ignores_ranges_and_returns_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new).ignore_range());
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let error = download(
            &server.url("/new.AppImage.zsync"),
            &seed,
            &mut context,
            &HttpClient::new(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Http(_)), "unexpected error: {error}");
        assert!(!has_zsync_temp(directory.path()));
    }

    #[test]
    fn rejects_a_malformed_control_file() {
        let directory = tempfile::tempdir().unwrap();
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, pattern(FILE_SIZE)).unwrap();

        let server = delta_server(
            b"not a control file".to_vec(),
            Route::new(pattern(FILE_SIZE)),
        );
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let error = download(
            &server.url("/new.AppImage.zsync"),
            &seed,
            &mut context,
            &HttpClient::new(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Http(_)), "unexpected error: {error}");
        assert!(!has_zsync_temp(directory.path()));
    }

    #[test]
    fn rejects_a_checksum_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, &old).unwrap();

        let mut control = ControlFile::generate(
            &mut Cursor::new(&new),
            "new.AppImage",
            "new.AppImage",
            Some(BLOCK_SIZE),
        )
        .unwrap();
        control.sha1 = Some("0".repeat(40));
        let mut control_file = Vec::new();
        control.write(&mut control_file).unwrap();

        let server = delta_server(control_file, Route::new(new));
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let error = download(
            &server.url("/new.AppImage.zsync"),
            &seed,
            &mut context,
            &HttpClient::new(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Http(_)), "unexpected error: {error}");
        assert!(!has_zsync_temp(directory.path()));
    }

    #[test]
    fn rejects_a_missing_seed_without_touching_the_network() {
        let directory = tempfile::tempdir().unwrap();
        let new = pattern(FILE_SIZE);
        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new));
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let error = download(
            &server.url("/new.AppImage.zsync"),
            &directory.path().join("missing.AppImage"),
            &mut context,
            &HttpClient::new(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Http(_)), "unexpected error: {error}");
        assert!(server.requests().is_empty());
    }

    #[test]
    fn rejects_a_directory_seed() {
        let directory = tempfile::tempdir().unwrap();
        let new = pattern(FILE_SIZE);
        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new));
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let error = download(
            &server.url("/new.AppImage.zsync"),
            directory.path(),
            &mut context,
            &HttpClient::new(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Http(_)), "unexpected error: {error}");
        assert!(server.requests().is_empty());
    }

    #[test]
    fn falls_back_without_range_requests_when_nothing_matches() {
        let directory = tempfile::tempdir().unwrap();
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, vec![0xFF; FILE_SIZE]).unwrap();
        let new = pattern(FILE_SIZE);
        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new));
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let error = download(
            &server.url("/new.AppImage.zsync"),
            &seed,
            &mut context,
            &HttpClient::new(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Http(_)), "unexpected error: {error}");
        assert_eq!(server.range_requests(), 0);
        assert!(!has_zsync_temp(directory.path()));
    }

    #[test]
    fn retries_a_transient_range_failure() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new.clone()).fail_first_range());
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let result = download(
            &server.url("/new.AppImage.zsync"),
            &seed,
            &mut context,
            &HttpClient::new(),
        )
        .unwrap();

        assert_eq!(fs::read(result).unwrap(), new);
        assert!(server.range_requests() >= 2);
    }

    #[test]
    fn cancels_a_download_in_progress() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(512 * 1024);
        let mut new = old.clone();
        for block in (0..new.len()).step_by(2 * BLOCK_SIZE) {
            new[block] = new[block].wrapping_add(1);
        }
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new).slow());
        let cancel = Arc::new(AtomicBool::new(false));
        let range_seen = server.range_seen();
        let canceller = {
            let cancel = Arc::clone(&cancel);
            thread::spawn(move || {
                for _ in 0..500 {
                    if range_seen.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                cancel.store(true, Ordering::Relaxed);
            })
        };
        let mut progress = |_| {};
        let mut context = context(directory.path(), &mut progress, Arc::clone(&cancel));

        let result = download(
            &server.url("/new.AppImage.zsync"),
            &seed,
            &mut context,
            &HttpClient::new(),
        );
        canceller.join().unwrap();

        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(
            server.range_requests() > 0,
            "cancellation must happen after the range download starts"
        );
        assert!(!has_zsync_temp(directory.path()));
    }

    #[test]
    fn stops_before_the_network_when_already_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let new = pattern(FILE_SIZE);
        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new));
        let cancel = Arc::new(AtomicBool::new(true));
        let mut progress = |_| {};
        let mut context = context(directory.path(), &mut progress, cancel);

        let result = download(
            &server.url("/new.AppImage.zsync"),
            &directory.path().join("old.AppImage"),
            &mut context,
            &HttpClient::new(),
        );

        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(server.requests().is_empty());
    }

    #[test]
    fn reconstructs_a_real_appimage() {
        let Some(source) = std::env::var_os("GEARLEVER_TEST_APPIMAGE").map(PathBuf::from) else {
            return;
        };
        let old = fs::read(&source).unwrap();
        if old.len() < 2 * BLOCK_SIZE {
            return;
        }

        let mut new = old.clone();
        let middle = new.len() / 2;
        for byte in &mut new[middle..middle + BLOCK_SIZE] {
            *byte = byte.wrapping_add(1);
        }

        let directory = tempfile::tempdir().unwrap();
        let seed = directory.path().join("old.AppImage");
        fs::write(&seed, &old).unwrap();
        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new.clone()));
        let mut progress = |_| {};
        let cancel = Arc::new(AtomicBool::new(false));
        let mut context = context(directory.path(), &mut progress, cancel);

        let result = download(
            &server.url("/new.AppImage.zsync"),
            &seed,
            &mut context,
            &HttpClient::new(),
        )
        .unwrap();

        assert_eq!(fs::read(result).unwrap(), new);
    }

    fn has_zsync_temp(directory: &Path) -> bool {
        let mut pending = vec![directory.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.filter_map(std::result::Result::ok) {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else if entry.file_name().to_string_lossy().ends_with(".zsync-tmp") {
                    return true;
                }
            }
        }
        false
    }
}
