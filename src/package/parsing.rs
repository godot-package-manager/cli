use crate::archive::*;
use crate::conversions::*;
use crate::package::{Manifest, Package};
use crate::Client;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use semver_rs::Version;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt};

#[derive(Clone, Debug, Default)]
pub struct ParsedPackage {
    pub name: String,
    pub version: VersionType,
}

#[derive(Clone, Debug, Default)]
pub enum VersionType {
    /// Normal version, just use it
    Normal(String),
    /// Abstract version, figure it out later
    #[default]
    Latest,
}

impl fmt::Display for VersionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                VersionType::Normal(v) => v,
                VersionType::Latest => "latest",
            }
        )
    }
}

impl ParsedPackage {
    /// Turn into a [Package].
    pub async fn into_package(self, client: Client) -> Result<Package> {
        match self.version {
            VersionType::Normal(v) => Package::new(self.name, v, client).await,
            VersionType::Latest => Package::new_no_version(self.name, client).await,
        }
    }
}

impl fmt::Display for ParsedPackage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

impl std::str::FromStr for ParsedPackage {
    type Err = anyhow::Error;

    /// Supports 3 version syntax variations: `:`, `=`, `@`, if version not specified, will fetch latest.
    /// see https://docs.npmjs.com/cli/v7/configuring-npm/package-json#name
    fn from_str(s: &str) -> Result<Self> {
        #[inline]
        fn not_too_long(s: &str) -> bool {
            s.len() < 214
        }
        #[inline]
        fn safe(s: &str) -> bool {
            s.find([
                ' ', '<', '>', '[', ']', '{', '}', '|', '\\', '^', '%', ':', '=',
            ])
            .is_none()
        }
        fn check(s: &str) -> Result<()> {
            if not_too_long(s) && safe(s) {
                Ok(())
            } else {
                Err(anyhow!("Invalid package name"))
            }
        }

        fn split_p(s: &str, d: char) -> Result<ParsedPackage> {
            let Some((p, v)) = s.split_once(d) else {
                check(s)?;
                return Ok(ParsedPackage {name: s.to_string(), version: VersionType::Latest });
            };
            check(p)?;
            Ok(ParsedPackage {
                name: p.to_string(),
                version: VersionType::Normal(v.to_string()),
            })
        }
        if s.contains(':') {
            // @bendn/gdcli:1.2.5
            split_p(s, ':')
        } else if s.contains('=') {
            // @bendn/gdcli=1.2.5
            return split_p(s, '=');
        } else {
            // @bendn/gdcli@1.2.5
            if s.as_bytes()[0] == b'@' {
                let mut owned_s = s.to_string();
                owned_s.remove(0);
                let Some((p, v)) = owned_s.split_once('@') else {
                    check(s)?;
                    return Ok(ParsedPackage {name: s.to_string(), version: VersionType::Latest });
                };
                check(&format!("@{p}")[..])?;
                return Ok(ParsedPackage {
                    name: format!("@{p}"),
                    version: VersionType::Normal(v.to_string()),
                });
            }
            return split_p(s, '@');
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct ParsedManifest {
    pub dist: ParsedManifestDist,
    #[serde(default)]
    pub dependencies: HashMap<String, String>,
    pub version: String,
}

impl fmt::Debug for ParsedManifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ParsedManifest<{:?}>", self.dependencies)
    }
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct ParsedManifestDist {
    pub shasum: String,
    pub tarball: String,
}

#[async_trait::async_trait]
impl TryFromAsync<ParsedManifest> for Manifest {
    async fn try_from_async(value: ParsedManifest, client: Client) -> Result<Manifest> {
        Ok(Manifest {
            shasum: Some(value.dist.shasum),
            tarball: CompressionType::Gzip(Data::new_uri(value.dist.tarball)),
            version: Version::new(&value.version).parse()?,
            dependencies: value.dependencies.try_into_async(client).await?,
        })
    }
}

#[derive(Serialize)]
pub struct Packument {
    pub versions: Vec<ParsedManifest>, // note: unprocessed manifests because we dont want to make requests for versions we dont need
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct ParsedPackument {
    pub versions: HashMap<String, ParsedManifest>,
}

impl From<ParsedPackument> for Packument {
    fn from(val: ParsedPackument) -> Self {
        let mut versions: Vec<ParsedManifest> = val.versions.into_values().collect();
        // sort newest first (really badly)
        versions.sort_unstable_by(|a, b| {
            Version::new(&b.version)
                .parse()
                .unwrap()
                .cmp(&Version::new(&a.version).parse().unwrap())
        });
        Packument { versions }
    }
}

#[async_trait]
impl TryFromAsync<HashMap<String, String>> for Vec<Package> {
    async fn try_from_async(
        value: HashMap<String, String>,
        client: Client,
    ) -> Result<Vec<Package>> {
        stream::iter(value.into_iter())
            .map(|(name, version)| async {
                let client = client.clone();
                async move {
                    let mut r = Package::new(name.clone(), version.clone(), client).await;
                    if let Ok(p) = &mut r {
                        p.indirect = true;
                    }
                    r
                }
                .await
            })
            .buffer_unordered(crate::PARALLEL)
            .collect::<Vec<Result<Package>>>()
            .await
            .into_iter()
            .collect()
    }
}

#[async_trait]
impl TryFromAsync<Vec<ParsedPackage>> for Vec<Package> {
    async fn try_from_async(value: Vec<ParsedPackage>, client: Client) -> Result<Vec<Package>> {
        stream::iter(value.into_iter())
            .map(|pp| async {
                let client = client.clone();
                async move { pp.into_package(client).await }.await
            })
            .buffer_unordered(crate::PARALLEL)
            .collect::<Vec<Result<Package>>>()
            .await
            .into_iter()
            .collect()
    }
}
