//! Direct HTTP(S) downloads and embedded `zsync|` update information.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::hash::HashCache;

use super::http::HttpClient;
use super::{DownloadContext, ManagerKind, UpdateManager, UpdateStatus, value, zsync};

/// A direct URL or `zsync|` update source.
#[derive(Debug, Clone)]
pub struct StaticFile {
    image: PathBuf,
    config: BTreeMap<String, String>,
    embedded: Option<String>,
    http: HttpClient,
    cache: super::TargetCache<(String, Option<zsync::ZsyncInfo>)>,
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

    fn target(&self) -> Result<Option<(String, Option<zsync::ZsyncInfo>)>> {
        if let Some(target) = self.cache.get() {
            return Ok(Some(target));
        }

        let target = self.resolve_target()?;
        if let Some(target) = &target {
            self.cache.set(target.clone());
        }

        Ok(target)
    }

    /// Resolves the URL to download and, for embedded sources, the parsed
    /// zsync header.
    fn resolve_target(&self) -> Result<Option<(String, Option<zsync::ZsyncInfo>)>> {
        if let Some(zsync_url) = self.embedded_url() {
            let info = zsync::parse(&self.http.get_text(&zsync_url)?);
            return Ok(Some((zsync::download_url(&zsync_url, &info), Some(info))));
        }

        Ok(self.configured_url().map(|url| (url, None)))
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
        let Some((url, zsync)) = self.target()? else {
            return Ok(UpdateStatus::Unknown);
        };
        let local_size = fs::metadata(&self.image).map(|metadata| metadata.len())?;

        if let Some(info) = zsync {
            if info.length.is_some_and(|length| length != local_size) {
                return Ok(UpdateStatus::Available);
            }

            if let Some(expected) = info.sha1 {
                let actual = hashes.sha1(&self.image)?;
                return Ok(if expected == actual {
                    UpdateStatus::UpToDate
                } else {
                    UpdateStatus::Available
                });
            }
        }

        let Some(remote_size) = self.http.content_length(&url)? else {
            return Ok(UpdateStatus::Unknown);
        };

        Ok(if remote_size == local_size {
            UpdateStatus::UpToDate
        } else {
            UpdateStatus::Available
        })
    }

    fn download(&self, context: &mut DownloadContext<'_>) -> Result<PathBuf> {
        let Some((url, _)) = self.target()? else {
            return Err(Error::UpdateConfig(
                "the update URL is not configured".to_owned(),
            ));
        };

        fs::create_dir_all(&context.directory)?;
        let path = context.output_path();
        self.http
            .download(&url, &path, &mut *context.progress, &context.cancel)?;

        Ok(path)
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
}
