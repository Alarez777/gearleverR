//! GitHub releases, including `gh-releases-zsync|` embedded information.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::hash::HashCache;

use super::http::HttpClient;
use super::{
    Asset, DownloadContext, ManagerKind, UpdateManager, UpdateStatus, boolean, pick_asset, value,
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
}

impl fmt::Debug for Github {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Github")
            .field("image", &self.image)
            .field("config", &self.config)
            .field("embedded", &self.embedded)
            .field("http", &self.http)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
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

struct Target {
    asset: Asset,
    zsync_url: Option<String>,
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

    fn fetch_target(&self) -> Result<Option<Target>> {
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

        if let Some(target_pattern) = pattern.strip_suffix(".zsync") {
            let zsync_asset = pick_asset(&assets, &pattern);
            let Some(asset) = pick_asset(&assets, target_pattern) else {
                return Ok(None);
            };

            return Ok(Some(Target {
                asset,
                zsync_url: zsync_asset.map(|asset| asset.url),
            }));
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
        let Some(target) = self.fetch_target()? else {
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
        let Some(target) = self.fetch_target()? else {
            return Err(Error::NoMatchingAsset(
                "no matching GitHub release".to_owned(),
            ));
        };

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
}
