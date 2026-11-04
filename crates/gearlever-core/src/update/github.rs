//! GitHub releases, including `gh-releases-zsync|` embedded information.

use std::collections::BTreeMap;
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

/// A GitHub release.
#[derive(Debug, Clone)]
pub struct Github {
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
    ) -> Self {
        Self {
            image,
            config,
            embedded,
            http,
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
            self.http.get_json(&base, Some(ACCEPT))?
        } else {
            vec![
                self.http
                    .get_json(&format!("{base}/latest"), Some(ACCEPT))?,
            ]
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
