//! GitHub releases, including `gh-releases-zsync|` embedded information.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::hash::HashCache;

use super::http::HttpClient;
use super::{
    Asset, DownloadContext, ManagerKind, UpdateManager, UpdateStatus, boolean, delta, pick_asset,
    value,
};

const ACCEPT: &str = "application/vnd.github+json";
const RATE_LIMIT_URL: &str = "https://api.github.com/rate_limit";
const USER_URL: &str = "https://api.github.com/user";

/// A GitHub release.
#[derive(Clone)]
pub struct Github {
    image: PathBuf,
    config: BTreeMap<String, String>,
    embedded: Option<String>,
    http: HttpClient,
    token: Option<String>,
    cache: super::TargetCache<Target>,
}

impl fmt::Debug for Github {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Github")
            .field("image", &self.image)
            .field("config", &self.config)
            .field("embedded", &self.embedded)
            .field("http", &self.http)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("cache", &self.cache)
            .finish()
    }
}

/// Result of checking a GitHub token against the rate-limit endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenCheck {
    /// The token was accepted.
    Valid {
        /// Remaining requests in the current window, when GitHub reports them.
        remaining: Option<u64>,
        /// Total requests allowed in the window, when GitHub reports them.
        limit: Option<u64>,
        /// Expiration reported by the `GitHub-Authentication-Token-Expiration`
        /// header, when present.
        expires_at: Option<String>,
    },
    /// GitHub rejected the credentials.
    Invalid,
    /// GitHub accepted the credentials but the request was forbidden.
    Forbidden,
    /// The rate limit is exhausted and the server asked to wait.
    RateLimited,
    /// GitHub returned an unexpected status.
    Unexpected(u16),
    /// The request could not be completed.
    Network(String),
}

/// Checks a GitHub token with `GET /rate_limit`.
///
/// The request runs synchronously and is meant to be called from a worker
/// thread. The expiration header is read from that response; when GitHub does
/// not send it there, a single `/user` request retrieves it.
#[must_use]
pub fn test_token(token: &str) -> TokenCheck {
    let client = HttpClient::shared();
    let result = client.get_json_with_token_expiration::<RateLimit>(
        RATE_LIMIT_URL,
        Some(ACCEPT),
        Some(token),
    );

    match result {
        Ok((rate_limit, mut expires_at)) => {
            if expires_at.is_none() {
                expires_at = client
                    .get_json_with_token_expiration::<serde_json::Value>(
                        USER_URL,
                        Some(ACCEPT),
                        Some(token),
                    )
                    .ok()
                    .and_then(|(_, expiration)| expiration);
            }

            let core = rate_limit.resources.and_then(|resources| resources.core);
            TokenCheck::Valid {
                remaining: core.as_ref().and_then(|core| core.remaining),
                limit: core.as_ref().and_then(|core| core.limit),
                expires_at,
            }
        }
        Err(Error::HttpStatus(status)) => match status.status {
            401 => TokenCheck::Invalid,
            403 if status.rate_limit_remaining == Some(0) || status.retry_after.is_some() => {
                TokenCheck::RateLimited
            }
            403 => TokenCheck::Forbidden,
            429 => TokenCheck::RateLimited,
            status => TokenCheck::Unexpected(status),
        },
        Err(error) => TokenCheck::Network(error.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    resources: Option<Resources>,
}

#[derive(Debug, Deserialize)]
struct Resources {
    core: Option<Core>,
}

#[derive(Debug, Deserialize)]
struct Core {
    limit: Option<u64>,
    remaining: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Clone)]
struct Target {
    asset: Asset,
    zsync_url: Option<String>,
}

/// Pairs the `.zsync` asset selected by `pattern` with the AppImage it describes.
fn pick_zsync_target(assets: &[Asset], pattern: &str) -> Option<Target> {
    let zsync_asset = pick_asset(assets, pattern)?;
    let target_name = Path::new(&zsync_asset.name).file_stem()?.to_str()?;
    let asset = assets
        .iter()
        .find(|asset| asset.name == target_name)?
        .clone();

    Some(Target {
        asset,
        zsync_url: Some(zsync_asset.url),
    })
}

impl Github {
    /// Creates the manager.
    #[must_use]
    pub fn new(
        image: PathBuf,
        config: BTreeMap<String, String>,
        embedded: Option<String>,
        http: HttpClient,
        token: Option<&str>,
    ) -> Self {
        Self {
            image,
            config,
            embedded,
            http,
            token: token.map(str::to_owned),
            cache: super::TargetCache::new(),
        }
    }

    fn embedded_data(&self) -> Option<(String, String, String)> {
        let raw = self
            .embedded
            .as_deref()?
            .strip_prefix("gh-releases-zsync|")?;
        let parts: Vec<&str> = raw.split('|').collect();
        if parts.len() != 4 {
            return None;
        }

        Some((
            format!("{}/{}", parts[0], parts[1]),
            parts[2].to_owned(),
            parts[3].to_owned(),
        ))
    }

    fn allow_prereleases(&self, embedded_release: Option<&str>) -> bool {
        if let Some(release) = embedded_release {
            return matches!(release, "latest-pre" | "latest-all");
        }

        boolean(&self.config, "allow_prereleases")
    }

    fn fetch_releases(&self, url: &str) -> Result<Vec<Release>> {
        self.fetch_json(url)
    }

    fn fetch_latest_release(&self, url: &str) -> Result<Release> {
        self.fetch_json(url)
    }

    fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.http
            .get_json_with_token(url, Some(ACCEPT), self.token.as_deref())
            .map_err(|error| self.map_api_error(error))
    }

    fn map_api_error(&self, error: Error) -> Error {
        if self.token.is_some()
            && matches!(&error, Error::HttpStatus(status) if status.status == 401)
        {
            Error::GithubTokenInvalid
        } else {
            error
        }
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

    fn resolve_target(&self) -> Result<Option<Target>> {
        let (repo, pattern, allow_prereleases) =
            if let Some((repo, release, pattern)) = self.embedded_data() {
                (repo, pattern, self.allow_prereleases(Some(&release)))
            } else {
                let repo = value(&self.config, "repo")
                    .ok_or_else(|| Error::UpdateConfig("missing repo".to_owned()))?
                    .to_owned();
                let pattern = value(&self.config, "repo_filename")
                    .ok_or_else(|| Error::UpdateConfig("missing repo_filename".to_owned()))?
                    .to_owned();
                (repo, pattern, self.allow_prereleases(None))
            };

        if !repo.contains('/') {
            return Err(Error::UpdateConfig(format!(
                "invalid GitHub repo {repo}, expected owner/name"
            )));
        }

        let base = format!("https://api.github.com/repos/{repo}/releases");
        let releases: Vec<Release> = if allow_prereleases {
            self.fetch_releases(&base)?
        } else {
            vec![self.fetch_latest_release(&format!("{base}/latest"))?]
        };

        let release = releases
            .into_iter()
            .find(|release| !release.draft && (allow_prereleases || !release.prerelease));

        let Some(release) = release else {
            return Ok(None);
        };

        let assets: Vec<Asset> = release
            .assets
            .into_iter()
            .map(|asset| Asset {
                name: asset.name,
                url: asset.browser_download_url,
                size: Some(asset.size),
                digest: asset.digest,
            })
            .collect();

        if Path::new(&pattern)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zsync"))
        {
            return Ok(pick_zsync_target(&assets, &pattern));
        }

        Ok(pick_asset(&assets, &pattern).map(|asset| Target {
            asset,
            zsync_url: None,
        }))
    }
}

impl UpdateManager for Github {
    fn kind(&self) -> ManagerKind {
        ManagerKind::Github
    }

    fn embedded(&self) -> Option<&str> {
        self.embedded.as_deref()
    }

    fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    fn check(&self, hashes: &HashCache) -> Result<UpdateStatus> {
        let Some(target) = self.target()? else {
            return Ok(if value(&self.config, "repo_filename").is_some() {
                UpdateStatus::Unknown
            } else {
                UpdateStatus::UpToDate
            });
        };

        let local_size = fs::metadata(&self.image).map(|metadata| metadata.len())?;
        if target.asset.size.is_some_and(|size| size != local_size) {
            return Ok(UpdateStatus::Available);
        }

        if let Some(zsync_url) = &target.zsync_url {
            let info = super::zsync::parse(&self.http.get_text(zsync_url)?);
            if let Some(expected) = info.sha1 {
                let actual = hashes.sha1(&self.image)?;
                return Ok(if expected == actual {
                    UpdateStatus::UpToDate
                } else {
                    UpdateStatus::Available
                });
            }
        }

        if let Some(digest) = target.asset.digest.as_deref()
            && let Some(expected) = digest.strip_prefix("sha256:")
        {
            let actual = hashes.sha256(&self.image)?;
            return Ok(if expected.eq_ignore_ascii_case(&actual) {
                UpdateStatus::UpToDate
            } else {
                UpdateStatus::Available
            });
        }

        Ok(if target.asset.size == Some(local_size) {
            UpdateStatus::UpToDate
        } else {
            UpdateStatus::Available
        })
    }

    fn download(&self, context: &mut DownloadContext<'_>) -> Result<PathBuf> {
        let Some(target) = self.target()? else {
            return Err(Error::NoMatchingAsset(
                "no matching GitHub release".to_owned(),
            ));
        };

        self.download_target(&target, context)
    }
}

impl Github {
    /// Downloads `target`, trying its zsync asset first when delta is enabled.
    fn download_target(
        &self,
        target: &Target,
        context: &mut DownloadContext<'_>,
    ) -> Result<PathBuf> {
        if context.use_zsync_delta
            && let Some(zsync_url) = target.zsync_url.as_deref()
        {
            match delta::download(zsync_url, &self.image, context, &self.http) {
                Ok(path) => return Ok(path),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => {
                    tracing::debug!(
                        image = %self.image.display(),
                        %error,
                        "zsync delta failed, downloading the full GitHub asset"
                    );
                }
            }
        }

        fs::create_dir_all(&context.directory)?;
        let path = context.output_path();
        self.http.download(
            &target.asset.url,
            &path,
            &mut *context.progress,
            &context.cancel,
        )?;

        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    use super::*;
    use crate::update::test_fixtures::{
        FILE_SIZE, changed_copy, control_bytes, delta_server, pattern,
    };
    use crate::update::test_server::{Route, TestServer};

    fn serve(body: String) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        (format!("http://{address}/releases/latest"), server)
    }

    fn manager() -> Github {
        Github::new(
            PathBuf::new(),
            BTreeMap::new(),
            None,
            HttpClient::new(),
            None,
        )
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_owned(),
            url: format!("https://example.com/{name}"),
            size: Some(1),
            digest: None,
        }
    }

    #[test]
    fn pairs_a_wildcard_zsync_with_its_appimage() {
        let assets = [
            "kdiskmark-3.3.0-1.aarch64.rpm",
            "kdiskmark-3.3.0-1.x86_64.rpm",
            "KDiskMark-3.3.0-fio-3.42-x86_64.AppImage",
            "KDiskMark-3.3.0-fio-3.42-x86_64.AppImage.sig",
            "KDiskMark-3.3.0-fio-3.42-x86_64.AppImage.zsync",
            "kdiskmark-3.3.0-source.tar.gz",
            "kdiskmark_3.3.0_amd64.deb",
            "kdiskmark_3.3.0_arm64.deb",
            "package-signing-key.pub",
        ]
        .map(asset);

        let target = pick_zsync_target(&assets, "*.zsync").unwrap();

        assert_eq!(
            target.asset.name,
            "KDiskMark-3.3.0-fio-3.42-x86_64.AppImage"
        );
        assert_eq!(
            target.asset.url,
            "https://example.com/KDiskMark-3.3.0-fio-3.42-x86_64.AppImage"
        );
        assert_eq!(
            target.zsync_url.as_deref(),
            Some("https://example.com/KDiskMark-3.3.0-fio-3.42-x86_64.AppImage.zsync")
        );
    }

    #[test]
    fn fetches_a_single_latest_release() {
        let (url, server) = serve(r#"{"draft":false,"prerelease":false,"assets":[]}"#.to_owned());
        let release = manager().fetch_latest_release(&url).unwrap();

        assert!(release.assets.is_empty());
        server.join().unwrap();
    }

    #[test]
    fn fetches_a_release_list() {
        let (url, server) = serve(r#"[{"draft":false,"prerelease":false,"assets":[]}]"#.to_owned());
        let releases = manager().fetch_releases(&url).unwrap();

        assert_eq!(releases.len(), 1);
        server.join().unwrap();
    }

    #[test]
    fn parses_the_rate_limit_resource() {
        let json = r#"{"resources":{"core":{"limit":5000,"remaining":4999}}}"#;
        let rate: RateLimit = serde_json::from_str(json).unwrap();
        let core = rate.resources.and_then(|resources| resources.core).unwrap();

        assert_eq!(core.limit, Some(5000));
        assert_eq!(core.remaining, Some(4999));
    }

    #[test]
    fn tolerates_an_empty_rate_limit_body() {
        let rate: RateLimit = serde_json::from_str("{}").unwrap();
        assert!(rate.resources.is_none());
    }

    #[test]
    fn downloads_through_the_zsync_asset_when_enabled() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new.clone()));
        let manager = Github::new(image, BTreeMap::new(), None, HttpClient::new(), None);
        let target = target(&server);
        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);
        context.use_zsync_delta = true;

        let downloaded = manager.download_target(&target, &mut context).unwrap();

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert!(server.range_requests() > 0);
    }

    #[test]
    fn skips_the_zsync_asset_when_delta_is_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new.clone()));
        let manager = Github::new(image, BTreeMap::new(), None, HttpClient::new(), None);
        let target = target(&server);
        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);

        let downloaded = manager.download_target(&target, &mut context).unwrap();

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert_eq!(server.range_requests(), 0);
        assert!(server.full_requests("/new.AppImage") >= 1);
    }

    #[test]
    fn falls_back_to_the_asset_when_delta_fails() {
        let directory = tempfile::tempdir().unwrap();
        let old = pattern(FILE_SIZE);
        let new = changed_copy(&old);
        let image = directory.path().join("App.AppImage");
        fs::write(&image, &old).unwrap();

        let control = control_bytes(&new, "new.AppImage");
        let server = delta_server(control, Route::new(new.clone()).ignore_range());
        let manager = Github::new(image, BTreeMap::new(), None, HttpClient::new(), None);
        let target = target(&server);
        let mut progress = |_fraction: f64| {};
        let mut context = DownloadContext::new(directory.path().join("downloads"), &mut progress);
        context.use_zsync_delta = true;

        let downloaded = manager.download_target(&target, &mut context).unwrap();

        assert_eq!(fs::read(downloaded).unwrap(), new);
        assert!(server.range_requests() > 0);
        assert!(server.full_requests("/new.AppImage") >= 1);
    }

    fn target(server: &TestServer) -> Target {
        Target {
            asset: Asset {
                name: "App.AppImage".to_owned(),
                url: server.url("/new.AppImage"),
                size: Some(u64::try_from(FILE_SIZE).unwrap()),
                digest: None,
            },
            zsync_url: Some(server.url("/new.AppImage.zsync")),
        }
    }
}
