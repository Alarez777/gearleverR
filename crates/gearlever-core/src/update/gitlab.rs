//! GitLab releases.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};
use crate::hash::HashCache;

use super::http::HttpClient;
use super::{Asset, DownloadContext, ManagerKind, UpdateManager, UpdateStatus, pick_asset, value};

/// A GitLab release source.
#[derive(Debug, Clone)]
pub struct Gitlab {
    image: PathBuf,
    config: BTreeMap<String, String>,
    embedded: Option<String>,
    http: HttpClient,
}

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    assets: Assets,
}

#[derive(Debug, Default, Deserialize)]
struct Assets {
    #[serde(default)]
    links: Vec<Link>,
}

#[derive(Debug, Deserialize)]
struct Link {
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    direct_asset_url: Option<String>,
}

impl Gitlab {
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
        }
    }

    fn remote(&self) -> Result<(String, String)> {
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
                "repo_url must point to a GitLab project".to_owned(),
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

    fn target(&self) -> Result<Option<Asset>> {
        let (host, project) = self.remote()?;
        let pattern = value(&self.config, "repo_filename")
            .ok_or_else(|| Error::UpdateConfig("missing repo_filename".to_owned()))?;

        let encoded = urlencoding::encode(&project);
        let url = format!("{host}/api/v4/projects/{encoded}/releases");
        let releases: Vec<Release> = self.http.get_json(&url, None)?;

        let Some(release) = releases.into_iter().next() else {
            return Ok(None);
        };

        let assets: Vec<Asset> = release
            .assets
            .links
            .into_iter()
            .map(|link| Asset {
                name: link.name,
                url: link.direct_asset_url.unwrap_or(link.url),
                size: None,
                digest: None,
            })
            .collect();

        Ok(pick_asset(&assets, pattern))
    }
}

impl UpdateManager for Gitlab {
    fn kind(&self) -> ManagerKind {
        ManagerKind::Gitlab
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

        let Some(remote_size) = self.http.content_length(&asset.url)? else {
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
        let Some(asset) = self.target()? else {
            return Err(Error::NoMatchingAsset(
                "no matching GitLab release".to_owned(),
            ));
        };

        fs::create_dir_all(&context.directory)?;
        let path = context.output_path();
        self.http
            .download(&asset.url, &path, &mut *context.progress)?;

        Ok(path)
    }
}
