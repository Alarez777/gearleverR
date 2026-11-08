//! Direct HTTP(S) downloads and embedded `zsync|` update information.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::hash::HashCache;

use super::http::{HttpClient, MAX_CONTROL_BYTES};
use super::{
    DownloadContext, ManagerKind, ProgressTracker, UpdateManager, UpdateStatus, delta, value, zsync,
};

#[derive(Clone)]
struct Target {
    url: String,
    zsync: Option<zsync::ZsyncInfo>,
    zsync_url: Option<String>,
    control: Option<Arc<[u8]>>,
    control_unavailable: bool,
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("url", &self.url)
            .field("zsync", &self.zsync)
            .field("zsync_url", &self.zsync_url)
            .field(
                "control_bytes",
                &self.control.as_ref().map(|control| control.len()),
            )
            .field("control_unavailable", &self.control_unavailable)
            .finish()
    }
}

/// A direct URL or `zsync|` update source.
#[derive(Debug, Clone)]
pub struct StaticFile {
    image: PathBuf,
    config: BTreeMap<String, String>,
    embedded: Option<String>,
    http: HttpClient,
    cache: super::TargetCache<Target>,
}

impl StaticFile {
    /// Creates the manager.
    #[must_use]
    pub fn new(
        image: PathBuf,
        config: BTreeMap<String, String>,
        embedded: Option<String>,
        http: HttpClient,
    ) -> Self {
        Self {
            image,
            config,
            embedded,
            http,
            cache: super::TargetCache::new(),
        }
    }

    fn embedded_url(&self) -> Option<String> {
        self.embedded
            .as_deref()
            .and_then(|raw| raw.strip_prefix("zsync|"))
            .map(ToOwned::to_owned)
    }

    fn configured_url(&self) -> Option<String> {
        value(&self.config, "url").map(ToOwned::to_owned)
    }

    fn target(&self) -> Result<Option<Target>> {
        if let Some(target) = self.cache.get() {
            return Ok(Some(target));
        }

        let target = self.resolve_target()?;
        if let Some(target) = &target {
            self.cache.set(target.clone());
        }

        Ok(target)
    }

    /// Resolves the download URL and keeps the raw control file for a delta
    /// download.
    fn resolve_target(&self) -> Result<Option<Target>> {
        if let Some(zsync_url) = self.embedded_url() {
            let Some(control) = self.http.get_bytes_limited(&zsync_url, MAX_CONTROL_BYTES)? else {
                tracing::debug!("zsync control file is too large, using the default download URL");
                let url = zsync_url
                    .strip_suffix(".zsync")
                    .unwrap_or(&zsync_url)
                    .to_owned();
                return Ok(Some(Target {
                    url,
                    zsync: None,
                    zsync_url: None,
                    control: None,
                    control_unavailable: true,
                }));
            };
            let info = zsync::parse_bytes(&control);
            return Ok(Some(Target {
                url: zsync::download_url(&zsync_url, &info),
                zsync: Some(info),
                zsync_url: Some(zsync_url),
                control: Some(control.into()),
                control_unavailable: false,
            }));
        }

        Ok(self.configured_url().map(|url| Target {
            url,
            zsync: None,
            zsync_url: None,
            control: None,
            control_unavailable: false,
        }))
    }
}

impl UpdateManager for StaticFile {
    fn kind(&self) -> ManagerKind {
        ManagerKind::StaticFile
    }

    fn embedded(&self) -> Option<&str> {
        self.embedded.as_deref()
    }

    fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    fn check(&self, hashes: &HashCache) -> Result<UpdateStatus> {
        let Some(target) = self.target()? else {
            return Ok(UpdateStatus::Unknown);
        };
        let local_size = fs::metadata(&self.image).map(|metadata| metadata.len())?;

        if target.control_unavailable {
            let Some(remote_size) = self.http.content_length(&target.url)? else {
                return Ok(UpdateStatus::Unknown);
            };
            return Ok(if remote_size == local_size {
                UpdateStatus::Unknown
            } else {
                UpdateStatus::Available
            });
        }

        if let Some(info) = &target.zsync {
            if info.length.is_some_and(|length| length != local_size) {
                return Ok(UpdateStatus::Available);
            }

            if let Some(expected) = info.sha1.as_deref() {
                let actual = hashes.sha1(&self.image)?;
                return Ok(if expected == actual {
                    UpdateStatus::UpToDate
                } else {
                    UpdateStatus::Available
                });
            }
        }

        let Some(remote_size) = self.http.content_length(&target.url)? else {
            return Ok(UpdateStatus::Unknown);
        };

        Ok(if remote_size == local_size {
            UpdateStatus::UpToDate
        } else {
            UpdateStatus::Available
        })
    }

    fn download(&self, context: &mut DownloadContext<'_>) -> Result<PathBuf> {
        if context.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let Some(target) = self.target()? else {
            return Err(Error::UpdateConfig(
                "the update URL is not configured".to_owned(),
            ));
        };

        let output = context.output_path();
        let use_zsync_delta = context.use_zsync_delta;
        let directory = context.directory.clone();
        let cancel = Arc::clone(&context.cancel);
        let mut tracker = ProgressTracker::new(context.progress);

        if use_zsync_delta
            && let (Some(zsync_url), Some(control)) =
                (target.zsync_url.as_deref(), target.control.as_deref())
        {
            let delta_result = {
                let mut progress = |fraction| tracker.report(fraction);
                let mut delta_context = DownloadContext {
                    directory: directory.clone(),
                    progress: &mut progress,
                    cancel: Arc::clone(&cancel),
                    use_zsync_delta: true,
                };
                delta::download(
                    zsync_url,
                    control,
                    &self.image,
                    &mut delta_context,
                    &self.http,
                )
            };

            match delta_result {
                Ok(path) => return Ok(path),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => {
                    tracing::debug!(
                        image = %self.image.display(),
                        %error,
                        "zsync delta failed, downloading the full AppImage"
                    );
                }
            }
        }

        fs::create_dir_all(&directory)?;
        let mut progress = |fraction| tracker.report(fraction);
        self.http
            .download(&target.url, &output, &mut progress, &cancel)?;

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::update::test_fixtures::{
        BLOCK_SIZE, FILE_SIZE, changed_copy, control_bytes, delta_server, pattern,
    };
    use crate::update::test_server::{Route, TestServer};

    #[test]
    fn reuses_the_zsync_header_between_check_and_download() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("App.AppImage");
        fs::write(&image, b"old").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let zsync_requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let zsync_requests = Arc::clone(&zsync_requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buffer = [0_u8; 4096];
                            let read = stream.read(&mut buffer).unwrap_or(0);
                            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();

                            let (body, is_zsync): (&[u8], bool) =
                                if request.starts_with("GET /App.AppImage.zsync") {
                                    (b"zsync: 0.6.2\nLength: 999999\n\n", true)
                                } else {
                                    (b"new", false)
                                };
                            if is_zsync {
                                zsync_requests.fetch_add(1, Ordering::SeqCst);
                            }
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.write_all(body);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|http://{address}/App.AppImage.zsync")),
            HttpClient::new(),
        );
        let hashes = HashCache::new(directory.path().join("hashes"));

        assert_eq!(manager.check(&hashes).unwrap(), UpdateStatus::Available);

        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);
        let downloaded = manager.download(&mut context).unwrap();
        assert_eq!(fs::read(&downloaded).unwrap(), b"new");
        assert_eq!(
            zsync_requests.load(Ordering::SeqCst),
            1,
            "download must reuse the target resolved by check"
        );

        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn uses_a_delta_download_when_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(&control, Route::new(new.clone()));
        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
            HttpClient::new(),
        );
        let hashes = HashCache::new(directory.path().join("hashes"));
        assert_eq!(manager.check(&hashes).unwrap(), UpdateStatus::Available);
        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);
        context.use_zsync_delta = true;

        let downloaded = manager.download(&mut context).unwrap();

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert_eq!(
            server.request_count("/new.AppImage.zsync"),
            1,
            "download must reuse the control file fetched by check"
        );
        assert!(server.range_requests() > 0);
    }

    #[test]
    fn skips_the_delta_download_when_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(&control, Route::new(new.clone()));
        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
            HttpClient::new(),
        );
        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);

        let downloaded = manager.download(&mut context).unwrap();

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert_eq!(server.range_requests(), 0);
        assert!(server.full_requests("/new.AppImage") >= 1);
    }

    #[test]
    fn falls_back_to_the_full_download_when_delta_fails() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(&control, Route::new(new.clone()).ignore_range());
        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
            HttpClient::new(),
        );
        let mut progress_values = Vec::new();
        let downloaded = {
            let mut progress = |fraction: f64| progress_values.push(fraction);
            let mut context =
                DownloadContext::new(directory.path().join("downloads"), &mut progress);
            context.use_zsync_delta = true;
            manager.download(&mut context).unwrap()
        };

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert!(server.range_requests() > 0);
        assert!(server.full_requests("/new.AppImage") >= 1);
        assert!(
            progress_values.windows(2).all(|pair| pair[0] <= pair[1]),
            "fallback progress must be monotonic: {progress_values:?}"
        );
        assert_eq!(progress_values.last().copied(), Some(1.0));
    }

    #[test]
    fn falls_back_when_the_control_file_is_too_large() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = vec![b'x'; crate::update::http::MAX_CONTROL_BYTES + 1];
        let server = delta_server(&control, Route::new(new.clone()));
        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
            HttpClient::new(),
        );
        let hashes = HashCache::new(directory.path().join("hashes"));
        assert_eq!(manager.check(&hashes).unwrap(), UpdateStatus::Unknown);

        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);
        context.use_zsync_delta = true;

        let downloaded = manager.download(&mut context).unwrap();

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert_eq!(server.range_requests(), 0);
        assert!(server.full_requests("/new.AppImage") >= 1);
    }

    #[test]
    fn propagates_a_control_fetch_error() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let server = TestServer::start(vec![("/new.AppImage", Route::new(new))]);
        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
            HttpClient::new(),
        );
        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);
        context.use_zsync_delta = true;

        let error = manager.download(&mut context).unwrap_err();

        assert!(matches!(error, Error::HttpStatus(_)));
        assert_eq!(server.full_requests("/new.AppImage"), 0);
    }

    #[test]
    fn falls_back_when_the_control_file_is_malformed() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let server = delta_server(b"not a control file", Route::new(new.clone()));
        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
            HttpClient::new(),
        );
        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);
        context.use_zsync_delta = true;

        let downloaded = manager.download(&mut context).unwrap();

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert_eq!(server.range_requests(), 0);
        assert!(server.full_requests("/new.AppImage") >= 1);
    }

    #[test]
    fn propagates_cancellation_without_a_full_download() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(&control, Route::new(new));
        let manager = StaticFile::new(
            image,
            BTreeMap::new(),
            Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
            HttpClient::new(),
        );
        let mut progress = |_fraction: f64| {};
        let cancel = Arc::new(AtomicBool::new(true));
        let mut context =
            DownloadContext::with_cancel(directory.path().join("downloads"), &mut progress, cancel);
        context.use_zsync_delta = true;

        let error = manager.download(&mut context).unwrap_err();

        assert!(matches!(error, Error::Cancelled));
        assert_eq!(server.request_count("/new.AppImage.zsync"), 0);
        assert_eq!(server.full_requests("/new.AppImage"), 0);
    }

    #[test]
    fn reconstructs_a_real_appimage_and_falls_back_without_ranges() {
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

        for ignore_range in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let image = directory.path().join("App.AppImage");
            fs::write(&image, &old).unwrap();

            let control = control_bytes(&new, "new.AppImage");
            let route = if ignore_range {
                Route::new(new.clone()).ignore_range()
            } else {
                Route::new(new.clone())
            };
            let server = delta_server(&control, route);
            let manager = StaticFile::new(
                image,
                BTreeMap::new(),
                Some(format!("zsync|{}", server.url("/new.AppImage.zsync"))),
                HttpClient::new(),
            );
            let mut progress = |_fraction: f64| {};
            let mut context =
                DownloadContext::new(directory.path().join("downloads"), &mut progress);
            context.use_zsync_delta = true;

            let downloaded = manager.download(&mut context).unwrap();

            assert_eq!(fs::read(downloaded).unwrap(), new);
            if ignore_range {
                assert!(server.full_requests("/new.AppImage") >= 1);
            }
        }
    }
}
