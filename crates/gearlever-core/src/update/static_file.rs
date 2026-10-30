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

    /// Resolves the URL to download and, for embedded sources, the zsync URL.
    fn target(&self) -> Result<Option<(String, Option<String>)>> {
        if let Some(zsync_url) = self.embedded_url() {
            let info = zsync::parse(&self.http.get_text(&zsync_url)?);
            return Ok(Some((
                zsync::download_url(&zsync_url, &info),
                Some(zsync_url),
            )));
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
        let Some((url, zsync_url)) = self.target()? else {
            return Ok(UpdateStatus::Unknown);
        };

        if let Some(zsync_url) = zsync_url {
            let info = zsync::parse(&self.http.get_text(&zsync_url)?);
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
        let local_size = fs::metadata(&self.image).map(|metadata| metadata.len())?;

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
        self.http.download(&url, &path, &mut *context.progress)?;

        Ok(path)
    }
}
