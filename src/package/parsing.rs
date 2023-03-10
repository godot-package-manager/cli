use crate::package::{Manifest, Package};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use semver_rs::Version;
use serde::Deserialize;
use std::{collections::HashMap, fmt};

macro_rules! parse_version {
    ($ver: expr) => {
        VersionType::Normal(Version::new($ver).parse()?)
    };
}

#[derive(Clone, Debug)]
pub struct ParsedPackage {
    pub name: String,
    pub version: VersionType,
}

#[derive(Clone, Debug)]
pub enum VersionType {
    /// Normal version, just use it
    Normal(Version),
    /// Abstract version, figure it out later
    Latest,
}

impl fmt::Display for VersionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                VersionType::Normal(v) => v.to_string(),
                VersionType::Latest => "latest".to_string(),
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
            s.find(&[
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
                version: parse_version!(&v.to_string()),
            })
        }
        if s.contains(':') {
            // @bendn/gdcli:1.2.5
            return split_p(s, ':');
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
                    version: parse_version!(&v.to_string()),
                });
            }
            return split_p(s, '@');
        };
    }
}

#[derive(Clone, Default, Debug, Deserialize)]
pub struct ParsedManifest {
    dist: ParsedManifestDist,
    #[serde(default)]
    dependencies: HashMap<String, String>,
    version: String,
}

#[derive(Clone, Default, Debug, Deserialize)]
pub struct ParsedManifestDist {
    pub integrity: String,
    pub shasum: String,
    pub tarball: String,
}

impl ParsedManifest {
    pub async fn into_manifest(self, client: Client) -> Result<Manifest> {
        Ok(Manifest {
            integrity: self.dist.integrity,
            shasum: self.dist.shasum,
            tarball: self.dist.tarball,
            version: self.version,
            dependencies: self.dependencies.into_package_list(client).await?,
        })
    }
}

#[async_trait]
pub trait IntoPackageList {
    async fn into_package_list(self, client: Client) -> Result<Vec<Package>>;
}

#[async_trait]
impl IntoPackageList for HashMap<String, String> {
    async fn into_package_list(self, client: Client) -> Result<Vec<Package>> {
        let buf = stream::iter(self.into_iter())
            .map(|(name, version)| async {
                let client = client.clone();
                async move {
                    Package::new(name, Version::new(&version).parse().unwrap(), client).await
                }
                .await
            })
            .buffer_unordered(crate::PARALLEL);
        let mut packages = vec![];
        for p in buf.collect::<Vec<Result<Package>>>().await {
            let mut p = p?;
            p.indirect = true;
            packages.push(p);
        }
        Ok(packages)
    }
}

#[async_trait]
impl IntoPackageList for Vec<ParsedPackage> {
    /// Fake result implementation
    async fn into_package_list(self, client: Client) -> Result<Vec<Package>> {
        let buf = stream::iter(self.into_iter())
            .map(|pp| async {
                let client = client.clone();
                async move {
                    let name = pp.to_string();
                    pp.into_package(client)
                        .await
                        .unwrap_or_else(|_| panic!("Package {name} could not be parsed"))
                }
                .await
            })
            .buffer_unordered(4);
        Ok(buf.collect::<Vec<Package>>().await)
    }
}
