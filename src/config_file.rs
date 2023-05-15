use crate::ctx;
use crate::package::parsing::IntoPackageList;
use crate::package::Package;
use anyhow::{Context, Result};
use console::style;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub type Cache = Arc<Mutex<HashMap<String, HashMap<String, Package>>>>;

#[derive(Debug, Default, Clone)]
/// The config file: parsed from godot.package, usually.
/// Contains only a list of [Package]s, currently.
pub struct ConfigFile {
    pub packages: Vec<Package>,
    pub cache: Cache, // hooks: there are no hooks now
}

pub fn create_cache() -> Cache {
    Cache::new(Mutex::new(HashMap::new()))
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default)]
/// A wrapper to [ConfigFile]. This _is_ necessary.
/// Any alternatives will end up being more ugly than this. (trust me i tried)
/// There is no way to automatically deserialize the map into a vec.
struct ParsedConfig {
    // support NPM package.json files (also allows gpm -c package.json -u)
    #[serde(alias = "dependencies")]
    packages: HashMap<String, String>,
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

impl<'a> From<ConfigFile> for ParsedConfig {
    fn from(from: ConfigFile) -> Self {
        Self {
            packages: from
                .packages
                .into_iter()
                .map(|p| (p.name, p.version.to_string()))
                .collect(),
        }
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

    pub async fn into_configfile(self, client: Client, cache: Cache) -> ConfigFile {
        let mut packages = ctx!(
            self.packages.into_package_list(client, cache.clone()).await,
            "turning ParsedConfig into ConfigFile"
        )
        .unwrap();
        for mut p in &mut packages {
            p.indirect = false
        }
        ConfigFile {
            packages,
            cache: cache,
        }
    }
}

impl ConfigFile {
    pub fn print(self, t: ConfigType) -> String {
        let w = ParsedConfig::from(self);
        match t {
            ConfigType::JSON => serde_json::to_string_pretty(&w).unwrap(),
            ConfigType::YAML => serde_yaml::to_string(&w).unwrap(),
            ConfigType::TOML => toml::to_string_pretty(&w).unwrap(),
        }
    }

    /// Creates a new [ConfigFile] from the given text
    /// Panics if the file cant be parsed as toml, hjson or yaml.
    pub async fn new(contents: &String, client: Client, cache: Cache) -> Self {
        if contents.is_empty() {
            panic!("Empty CFG");
        }

        // definetly not going to backfire
        let mut cfg = if contents.as_bytes()[0] == b'{' {
            // json gets brute forced first so this isnt really needed
            Self::parse(contents, ConfigType::JSON, client, cache)
                .await
                .expect("Parsing CFG from JSON should work")
        } else if contents.len() > 3 && contents[..3] == *"---" {
            Self::parse(contents, ConfigType::YAML, client, cache)
                .await
                .expect("Parsing CFG from YAML should work")
        } else {
            for i in [ConfigType::JSON, ConfigType::YAML, ConfigType::TOML].into_iter() {
                let res = Self::parse(contents, i, client.clone(), cache.clone()).await;

                // im sure theres some kind of idiomatic rust way to do this that i dont know of
                if res.is_ok() {
                    return res.unwrap();
                }

                println!(
                    "{:>12} Parsing CFG from {:#?} failed: `{}` (ignore if cfg not written in {:#?})",
                    crate::putils::warn(),
                    i,
                    style(res.unwrap_err()).red(),
                    i
                )
            }
            panic!("Parsing CFG failed (see above warnings to find out why)");
        };
        cfg.packages.sort();
        cfg
    }

    async fn parse(txt: &str, t: ConfigType, client: Client, cache: Cache) -> Result<ConfigFile> {
        Ok(ParsedConfig::parse(txt, t)?
            .into_configfile(client, cache)
            .await)
    }

    /// Creates a lockfile for this config file.
    /// note: Lockfiles are currently unused.
    pub fn lock(&mut self) -> String {
        let mut pkgs = vec![];
        for p in self.collect() {
            if p.is_installed() {
                pkgs.push(p)
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
    pub fn collect(&mut self) -> Vec<Package> {
        let mut pkgs: Vec<Package> = vec![];
        self.for_each(|p| pkgs.push(p.clone()));
        pkgs
    }
}

#[cfg(test)]
mod tests {
    use crate::config_file::*;

    #[tokio::test]
    async fn parse() {
        let _t = crate::test_utils::mktemp();
        let c = crate::mkclient();
        let cache = create_cache();
        let cfgs: [&mut ConfigFile; 3] = [
            &mut ConfigFile::new(
                &r#"dependencies: { "@bendn/test": 2.0.10 }"#.into(),
                c.clone(),
                cache.clone(),
            )
            .await,
            &mut ConfigFile::new(
                &"dependencies:\n  \"@bendn/test\": \"2.0.10\"".into(),
                c.clone(),
                cache.clone(),
            )
            .await,
            &mut ConfigFile::new(
                &"[dependencies]\n\"@bendn/test\" = \"2.0.10\"".into(),
                c.clone(),
                cache.clone(),
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
                p.download(c.clone()).await
            }
            assert_eq!(
                serde_json::from_str::<Vec<LockFileEntry>>(cfg.lock().as_str()).unwrap(),
                wanted_lockfile
            );
        }
    }
}
