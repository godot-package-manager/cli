use crate::conversions::*;
use crate::ctx;
use crate::package::Manifest;
use crate::package::Package;
use crate::Client;

use anyhow::{Context, Result};
use console::style;
use semver_rs::Version;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The config file: parsed from godot.package, usually.
#[derive(Default)]
pub struct ConfigFile {
    name: String,
    version: String,
    pub packages: Vec<Package>,
    // hooks: there are no hooks now
}

#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
/// A wrapper to [ConfigFile]. This _is_ necessary.
/// Any alternatives will end up being more ugly than this. (trust me i tried)
/// There is no way to automatically deserialize the map into a vec.
struct ParsedConfig {
    // support NPM package.json files (also allows gpm -c package.json -u)
    #[serde(alias = "dependencies")]
    packages: HashMap<String, String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigType {
    JSON,
    YAML,
    TOML,
}

impl std::fmt::Display for ConfigType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:#?}", self)
    }
}

impl From<&ConfigFile> for ParsedConfig {
    fn from(from: &ConfigFile) -> Self {
        Self {
            packages: from
                .packages
                .iter()
                .map(|p| (p.name.to_string(), p.manifest.version.to_string()))
                .collect(),
            name: String::new(),
            version: String::new(),
        }
    }
}

#[async_trait::async_trait]
impl TryFromAsync<ParsedConfig> for ConfigFile {
    async fn try_from_async(value: ParsedConfig, client: Client) -> Result<Self> {
        let mut packages: Vec<Package> = ctx!(
            value.packages.try_into_async(client).await,
            "turning ParsedConfig into ConfigFile"
        )
        .unwrap();
        for mut p in &mut packages {
            p.indirect = false
        }
        Ok(ConfigFile {
            packages,
            name: value.name,
            version: value.version,
        })
    }
}

impl ParsedConfig {
    pub fn parse(txt: &str, t: ConfigType) -> Result<Self> {
        Ok(match t {
            ConfigType::TOML => toml::from_str::<ParsedConfig>(txt)?,
            ConfigType::JSON => deser_hjson::from_str::<ParsedConfig>(txt)?,
            ConfigType::YAML => serde_yaml::from_str::<ParsedConfig>(txt)?,
        })
    }
}

impl ConfigFile {
    pub fn empty() -> Self {
        Self {
            packages: vec![],
            ..ConfigFile::default()
        }
    }

    pub fn print(&self, t: ConfigType) -> String {
        let w = ParsedConfig::from(self);
        match t {
            ConfigType::JSON => serde_json::to_string_pretty(&w).unwrap(),
            ConfigType::YAML => serde_yaml::to_string(&w).unwrap(),
            ConfigType::TOML => toml::to_string_pretty(&w).unwrap(),
        }
    }

    /// Creates a new [ConfigFile] from the given text
    /// Panics if the file cant be parsed as toml, hjson or yaml.
    pub async fn new(contents: &String, client: Client) -> Self {
        if contents.is_empty() {
            panic!("Empty CFG");
        }

        // definetly not going to backfire
        let mut cfg = if contents.as_bytes()[0] == b'{' {
            // json gets brute forced first so this isnt really needed
            Self::parse(contents, ConfigType::JSON, client)
                .await
                .expect("Parsing CFG from JSON should work")
        } else if contents.len() > 3 && contents[..3] == *"---" {
            Self::parse(contents, ConfigType::YAML, client)
                .await
                .expect("Parsing CFG from YAML should work")
        } else {
            for i in [ConfigType::JSON, ConfigType::YAML, ConfigType::TOML].into_iter() {
                let res = Self::parse(contents, i, client.clone()).await;

                if let Ok(parsed) = res {
                    return parsed;
                }

                println!(
                    "{:>12} Parsing CFG from {:#?} failed: `{}` (ignore if cfg not written in {:#?})",
                    crate::putils::warn(),
                    i,
                    style(res.err().unwrap()).red(),
                    i
                )
            }
            panic!("Parsing CFG failed (see above warnings to find out why)");
        };
        cfg.packages.sort();
        cfg
    }

    pub async fn parse(txt: &str, t: ConfigType, client: Client) -> Result<ConfigFile> {
        ParsedConfig::parse(txt, t)?.try_into_async(client).await
    }

    pub fn into_package(self, uri: crate::archive::CompressionType) -> Result<Package> {
        Ok(Package::from_manifest(
            Manifest {
                version: Version::new(&self.version).parse()?,
                shasum: None,
                tarball: uri,
                dependencies: self.packages,
            },
            self.name,
        ))
    }

    /// Creates a lockfile for this config file.
    /// note: Lockfiles are currently unused.
    pub fn lock(&mut self, cwd: &Path) -> String {
        let mut pkgs = vec![];
        for mut p in self.collect() {
            if p.is_installed(cwd) {
                p.prepare_lock();
                pkgs.push(p);
            };
        }
        pkgs.sort();
        serde_json::to_string_pretty(&pkgs).unwrap()
    }

    /// Iterates over all the packages (and their deps) in this config file.
    fn _for_each(pkgs: &mut [Package], mut cb: impl FnMut(&mut Package)) {
        fn inner(pkgs: &mut [Package], cb: &mut impl FnMut(&mut Package)) {
            for p in pkgs {
                cb(p);
                if p.has_deps() {
                    inner(&mut p.manifest.dependencies, cb);
                }
            }
        }
        inner(pkgs, &mut cb);
    }

    /// Public wrapper for _for_each, but with the initial value filled out.
    pub fn for_each(&mut self, cb: impl FnMut(&mut Package)) {
        Self::_for_each(&mut self.packages, cb)
    }

    /// Collect all the packages, and their dependencys.
    /// Uses clones, because I wasn't able to get references to work
    pub fn collect(&mut self) -> HashSet<Package> {
        let mut pkgs: HashSet<Package> = HashSet::new();
        self.for_each(|p| {
            pkgs.insert(p.clone());
        });
        pkgs
    }
}

#[cfg(test)]
mod tests {
    use crate::config_file::*;

    #[tokio::test]
    async fn parse() {
        let t = crate::test_utils::mktemp().await;
        let c = t.2;
        let cfgs: [&mut ConfigFile; 3] = [
            &mut ConfigFile::new(
                &r#"dependencies: { "@bendn/test": 2.0.10 }"#.into(),
                c.clone(),
            )
            .await,
            &mut ConfigFile::new(
                &"dependencies:\n  \"@bendn/test\": \"2.0.10\"".into(),
                c.clone(),
            )
            .await,
            &mut ConfigFile::new(
                &"[dependencies]\n\"@bendn/test\" = \"2.0.10\"".into(),
                c.clone(),
            )
            .await,
        ];
        #[derive(Debug, Deserialize, Clone, Eq, PartialEq)]
        struct LockFileEntry {
            pub name: String,
            pub version: String,
        }
        let wanted_lockfile = serde_json::from_str::<Vec<LockFileEntry>>(
            r#"[{"name":"@bendn/gdcli","version":"1.2.5"},{"name":"@bendn/test","version":"2.0.10"}]"#,
        ).unwrap();
        for cfg in cfgs {
            assert_eq!(cfg.packages.len(), 1);
            assert_eq!(cfg.packages[0].to_string(), "@bendn/test@2.0.10");
            assert_eq!(cfg.packages[0].manifest.dependencies.len(), 1);
            assert_eq!(
                cfg.packages[0].manifest.dependencies[0].to_string(),
                "@bendn/gdcli@1.2.5"
            );
            for mut p in cfg.collect() {
                p.download(c.clone(), t.0.path()).await
            }
            assert_eq!(
                serde_json::from_str::<Vec<LockFileEntry>>(cfg.lock(t.0.path()).as_str()).unwrap(),
                wanted_lockfile
            );
        }
    }
}
