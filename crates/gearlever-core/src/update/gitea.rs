//! Codeberg and Forgejo releases, which share the Gitea API.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};
use crate::hash::HashCache;

use super::http::HttpClient;
use super::{
    Asset, DownloadContext, ManagerKind, UpdateManager, UpdateStatus, boolean, pick_asset, value,
};

const CODEBERG: &str = "https://codeberg.org";

/// A Gitea-compatible release source (Codeberg or Forgejo).
#[derive(Debug, Clone)]
pub struct Gitea {
    kind: ManagerKind,
    image: PathBuf,
    config: BTreeMap<String, String>,
    embedded: Option<String>,
    http: HttpClient,
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
}

impl Gitea {
    /// Creates a Codeberg manager.
    #[must_use]
    pub fn codeberg(
        image: PathBuf,
        config: BTreeMap<String, String>,
        embedded: Option<String>,
        http: HttpClient,
    ) -> Self {
        Self::new(ManagerKind::Codeberg, image, config, embedded, http)
    }

    /// Creates a Forgejo manager.
    #[must_use]
    pub fn forgejo(
        image: PathBuf,
        config: BTreeMap<String, String>,
        embedded: Option<String>,
        http: HttpClient,
    ) -> Self {
        Self::new(ManagerKind::Forgejo, image, config, embedded, http)
    }

    fn new(
        kind: ManagerKind,
        image: PathBuf,
        config: BTreeMap<String, String>,
        embedded: Option<String>,
        http: HttpClient,
    ) -> Self {
        Self {
            kind,
            image,
            config,
            embedded,
            http,
        }
    }

    fn instance(&self) -> Result<(String, String)> {
        match self.kind {
            ManagerKind::Codeberg => {
                let repo = value(&self.config, "repo")
                    .ok_or_else(|| Error::UpdateConfig("missing repo".to_owned()))?;
                Ok((CODEBERG.to_owned(), repo.to_owned()))
            }
            ManagerKind::Forgejo => {
                let repo_url = value(&self.config, "repo_url")
                    .ok_or_else(|| Error::UpdateConfig("missing repo_url".to_owned()))?;
                let url = Url::parse(repo_url)
                    .map_err(|error| Error::UpdateConfig(format!("invalid repo_url: {error}")))?;
                let segments: Vec<&str> = url
                    .path_segments()
                    .map(std::iter::Iterator::collect)
                    .unwrap_or_default();
                if segments.len() < 2 {
                    return Err(Error::UpdateConfig(
                        "repo_url must point to a repository".to_owned(),
                    ));
                }
                let host = url
                    .host_str()
                    .ok_or_else(|| Error::UpdateConfig("repo_url has no host".to_owned()))?;

                Ok((
                    format!("https://{host}"),
                    format!("{}/{}", segments[0], segments[1]),
                ))
            }
            _ => Err(Error::UpdateConfig("unsupported manager".to_owned())),
        }
    }

    fn target(&self) -> Result<Option<Asset>> {
        let (base, repo) = self.instance()?;
        let pattern = value(&self.config, "repo_filename")
            .ok_or_else(|| Error::UpdateConfig("missing repo_filename".to_owned()))?;
        let allow_prereleases = boolean(&self.config, "allow_prereleases");

        let url = format!("{base}/api/v1/repos/{repo}/releases?limit=50");
        let releases: Vec<Release> = self.http.get_json(&url, None)?;

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
                digest: None,
            })
            .collect();

        Ok(pick_asset(&assets, pattern))
    }
}

impl UpdateManager for Gitea {
    fn kind(&self) -> ManagerKind {
        self.kind
    }

    fn embedded(&self) -> Option<&str> {
        self.embedded.as_deref()
    }

    fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    fn check(&self, _hashes: &HashCache) -> Result<UpdateStatus> {
        let Some(asset) = self.target()? else {
            return Ok(UpdateStatus::Unknown);
        };

        let local_size = fs::metadata(&self.image).map(|metadata| metadata.len())?;

        Ok(if asset.size == Some(local_size) {
            UpdateStatus::UpToDate
        } else {
            UpdateStatus::Available
        })
    }

    fn download(&self, context: &mut DownloadContext<'_>) -> Result<PathBuf> {
        let Some(asset) = self.target()? else {
            return Err(Error::NoMatchingAsset("no matching release".to_owned()));
        };

        fs::create_dir_all(&context.directory)?;
        let path = context.output_path();
        self.http
            .download(&asset.url, &path, &mut *context.progress)?;

        Ok(path)
    }
}
