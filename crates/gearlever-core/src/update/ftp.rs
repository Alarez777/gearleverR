//! FTP update sources.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use suppaftp::{FtpError, FtpStream};
use url::Url;

use crate::error::{Error, Result};
use crate::hash::HashCache;

use super::http::HttpClient;
use super::{DownloadContext, ManagerKind, UpdateManager, UpdateStatus, value};

const MAX_DEPTH: usize = 8;
/// Maximum time spent resolving and opening an FTP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time to wait for control or data traffic from the FTP server.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// An FTP server with a file name pattern.
#[derive(Debug, Clone)]
pub struct Ftp {
    image: PathBuf,
    config: BTreeMap<String, String>,
    embedded: Option<String>,
    #[expect(dead_code, reason = "kept uniform with the other update sources")]
    http: HttpClient,
    cache: super::TargetCache<(String, u64)>,
}

#[derive(Debug)]
struct Remote {
    host: String,
    port: u16,
    base: String,
    pattern: String,
}

#[derive(Debug)]
struct Entry {
    name: String,
    is_dir: bool,
}

impl Ftp {
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

    fn remote(&self) -> Result<Remote> {
        let raw = value(&self.config, "url")
            .ok_or_else(|| Error::UpdateConfig("missing FTP url".to_owned()))?;
        let url = Url::parse(raw)
            .map_err(|error| Error::UpdateConfig(format!("invalid FTP url: {error}")))?;

        if url.scheme() != "ftp" {
            return Err(Error::UpdateConfig(format!("{raw} is not an ftp:// URL")));
        }

        let host = url
            .host_str()
            .ok_or_else(|| Error::UpdateConfig("FTP url has no host".to_owned()))?;
        let pattern = value(&self.config, "filename")
            .ok_or_else(|| Error::UpdateConfig("missing FTP file name pattern".to_owned()))?;

        Ok(Remote {
            host: host.to_owned(),
            port: url.port().unwrap_or(21),
            base: url.path().trim_end_matches('/').to_owned(),
            pattern: pattern.to_owned(),
        })
    }

    fn connect(remote: &Remote) -> Result<FtpStream> {
        let addresses = (remote.host.as_str(), remote.port)
            .to_socket_addrs()
            .map_err(|error| Error::Http(format!("FTP resolve: {error}")))?;
        let mut last_error = None;
        let mut connected = None;
        for address in addresses {
            match FtpStream::connect_timeout(address, CONNECT_TIMEOUT) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let stream = connected.ok_or_else(|| {
            let reason =
                last_error.map_or_else(|| "no address".to_owned(), |error| error.to_string());
            Error::Http(format!("FTP connect: {reason}"))
        })?;

        {
            let socket = stream.get_ref();
            socket
                .set_read_timeout(Some(IO_TIMEOUT))
                .map_err(|error| Error::Http(format!("FTP control timeout: {error}")))?;
            socket
                .set_write_timeout(Some(IO_TIMEOUT))
                .map_err(|error| Error::Http(format!("FTP control timeout: {error}")))?;
        }

        let mut stream = stream.passive_stream_builder(|address| {
            let data = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
                .map_err(FtpError::ConnectionError)?;
            data.set_read_timeout(Some(IO_TIMEOUT))
                .map_err(FtpError::ConnectionError)?;
            data.set_write_timeout(Some(IO_TIMEOUT))
                .map_err(FtpError::ConnectionError)?;
            Ok(data)
        });
        stream
            .login("anonymous", "anonymous")
            .map_err(|error| Error::Http(format!("FTP login: {error}")))?;

        Ok(stream)
    }

    fn target(&self) -> Result<Option<(String, u64)>> {
        if let Some(target) = self.cache.get() {
            return Ok(Some(target));
        }

        let remote = self.remote()?;
        let mut stream = Self::connect(&remote)?;
        let target = self.find_file(&mut stream)?;
        let _ = stream.quit();

        if let Some(target) = &target {
            self.cache.set(target.clone());
        }

        Ok(target)
    }

    fn find_file(&self, stream: &mut FtpStream) -> Result<Option<(String, u64)>> {
        let remote = self.remote()?;
        let parts: Vec<&str> = remote
            .pattern
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();

        if parts.is_empty() {
            return Ok(None);
        }

        let wildcard = parts
            .iter()
            .position(|part| part.contains('*') || part.contains('?'))
            .unwrap_or(parts.len() - 1);

        let mut start = if remote.pattern.starts_with('/') || remote.base.is_empty() {
            String::from("/")
        } else {
            format!("{}/", remote.base.trim_end_matches('/'))
        };

        for part in &parts[..wildcard] {
            start = join(&start, part);
        }

        search(stream, &start, &parts[wildcard..], 0)
    }
}

impl UpdateManager for Ftp {
    fn kind(&self) -> ManagerKind {
        ManagerKind::Ftp
    }

    fn embedded(&self) -> Option<&str> {
        self.embedded.as_deref()
    }

    fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    fn check(&self, _hashes: &HashCache) -> Result<UpdateStatus> {
        let Some((_, remote_size)) = self.target()? else {
            return Ok(UpdateStatus::Unknown);
        };

        let local_size = fs::metadata(&self.image).map(|metadata| metadata.len())?;

        Ok(if remote_size == local_size {
            UpdateStatus::UpToDate
        } else {
            UpdateStatus::Available
        })
    }

    fn download(&self, context: &mut DownloadContext<'_>) -> Result<PathBuf> {
        let remote = self.remote()?;
        let Some((path, size)) = self.target()? else {
            return Err(Error::NoMatchingAsset(remote.pattern));
        };

        let mut stream = Self::connect(&remote)?;
        fs::create_dir_all(&context.directory)?;
        let destination = context.output_path();
        let file = File::create(&destination)?;
        let mut writer = ProgressWriter {
            file,
            written: 0,
            total: size,
            progress: &mut *context.progress,
            cancel: &context.cancel,
        };

        let transfer = stream.retr(&path, |reader| {
            io::copy(reader, &mut writer).map_err(suppaftp::FtpError::ConnectionError)?;
            Ok(())
        });

        if context.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = std::fs::remove_file(&destination);
            return Err(Error::Cancelled);
        }

        transfer.map_err(|error| Error::Http(format!("FTP download: {error}")))?;
        let _ = stream.quit();
        writer.file.flush()?;
        (writer.progress)(1.0);

        Ok(destination)
    }
}

fn search(
    stream: &mut FtpStream,
    directory: &str,
    parts: &[&str],
    depth: usize,
) -> Result<Option<(String, u64)>> {
    if depth > MAX_DEPTH || parts.is_empty() {
        return Ok(None);
    }

    let matcher = glob::Pattern::new(parts[0])
        .map_err(|error| Error::UpdateConfig(format!("invalid FTP pattern: {error}")))?;
    let listing = stream
        .list(Some(directory))
        .map_err(|error| Error::Http(format!("FTP list {directory}: {error}")))?;

    for line in listing {
        let Some(entry) = parse_list_line(&line) else {
            continue;
        };
        if !matcher.matches(&entry.name) {
            continue;
        }

        let path = join(directory, &entry.name);

        if parts.len() == 1 {
            if entry.is_dir {
                continue;
            }

            if let Ok(size) = stream.size(&path) {
                return Ok(Some((path, u64::try_from(size).unwrap_or(u64::MAX))));
            }
        } else if entry.is_dir
            && let Some(found) = search(stream, &path, &parts[1..], depth + 1)?
        {
            return Ok(Some(found));
        }
    }

    Ok(None)
}

fn parse_list_line(line: &str) -> Option<Entry> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 9 {
        return None;
    }

    let permissions = fields[0];
    if permissions.len() < 10 {
        return None;
    }

    let mut name = fields[8..].join(" ");
    if let Some(index) = name.find(" -> ") {
        name.truncate(index);
    }

    if name == "." || name == ".." {
        return None;
    }

    Some(Entry {
        name,
        is_dir: permissions.starts_with('d'),
    })
}

fn join(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", directory.trim_end_matches('/'))
    }
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "download progress does not need exact precision"
)]
fn fraction(written: u64, total: u64) -> f64 {
    (written as f64 / total as f64).min(1.0)
}

struct ProgressWriter<'a> {
    file: File,
    written: u64,
    total: u64,
    progress: &'a mut dyn FnMut(f64),
    cancel: &'a std::sync::atomic::AtomicBool,
}

impl Write for ProgressWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "download cancelled",
            ));
        }

        let written = self.file.write(buffer)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));

        if self.total > 0 {
            (self.progress)(fraction(self.written, self.total));
        }

        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_listings() {
        let entry = parse_list_line(
            "-rw-r--r--    1 ftp      ftp       1234 Jan 01 10:00 digiKam-8.0-x86-64.appimage",
        )
        .unwrap();
        assert_eq!(entry.name, "digiKam-8.0-x86-64.appimage");
        assert!(!entry.is_dir);

        let directory =
            parse_list_line("drwxr-xr-x    2 ftp ftp 4096 Jan 01 10:00 stable").unwrap();
        assert!(directory.is_dir);
        assert_eq!(directory.name, "stable");
    }

    #[test]
    fn ignores_entries_and_dot_directories() {
        assert!(parse_list_line("total 12").is_none());
        assert!(parse_list_line("drwxr-xr-x 2 ftp ftp 4096 Jan 01 10:00 .").is_none());
    }

    #[test]
    fn joins_paths_without_double_slashes() {
        assert_eq!(join("/", "pub"), "/pub");
        assert_eq!(join("/pub/", "app.AppImage"), "/pub/app.AppImage");
    }
}
