use crate::package::Package;
use anyhow::Result;
use console::style;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
/// The config file: parsed from godot.package, usually.
/// Contains only a list of [Package]s, currently.
pub struct ConfigFile {
    pub packages: Vec<Package>,
    // hooks: there are no hooks now
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default)]
/// A wrapper to [ConfigFile]. This _is_ necessary.
/// Any alternatives will end up being more ugly than this. (trust me i tried)
/// There is no way to automatically deserialize the map into a vec.
struct ConfigWrapper {
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

impl From<ConfigWrapper> for ConfigFile {
    fn from(from: ConfigWrapper) -> Self {
        Self {
            packages: from
                .packages
                .into_iter()
                .map(|(name, version)| Package::new(name, version).unwrap())
                .collect(),
        }
    }
}

impl From<ConfigFile> for ConfigWrapper {
    fn from(from: ConfigFile) -> Self {
        Self {
            packages: from
                .packages
                .into_iter()
                .map(|p| (p.name, p.version))
                .collect(),
        }
    }
}

impl ConfigFile {
    pub fn print(self, t: ConfigType) -> String {
        let w = ConfigWrapper::from(self);
        match t {
            ConfigType::JSON => serde_json::to_string_pretty(&w).unwrap(),
            ConfigType::YAML => serde_yaml::to_string(&w).unwrap(),
            ConfigType::TOML => toml::to_string_pretty(&w).unwrap(),
        }
    }

    /// Creates a new [ConfigFile] from the given text
    /// Panics if the file cant be parsed as toml, hjson or yaml.
    pub fn new(contents: &String) -> Self {
        if contents.len() == 0 {
            panic!("Empty CFG");
        }

        // definetly not going to backfire
        let mut cfg = if contents.as_bytes()[0] == b'{' {
            // json gets brute forced first so this isnt really needed
            Self::parse(contents, ConfigType::JSON).expect("Parsing CFG from JSON should work")
        } else if contents.len() > 3 && contents[..3] == *"---" {
            Self::parse(contents, ConfigType::YAML).expect("Parsing CFG from YAML should work")
        } else {
            for i in [ConfigType::JSON, ConfigType::YAML, ConfigType::TOML].into_iter() {
                let res = Self::parse(contents, i.clone());

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

    pub fn parse(txt: &String, t: ConfigType) -> Result<Self> {
        type W = ConfigWrapper;
        Ok(match t {
            ConfigType::TOML => toml::from_str::<W>(txt)?.into(),
            ConfigType::JSON => deser_hjson::from_str::<W>(txt)?.into(),
            ConfigType::YAML => serde_yaml::from_str::<W>(txt)?.into(),
        })
    }

    /// Creates a lockfile for this config file.
    /// note: Lockfiles are currently unused.
    pub fn lock(&mut self) -> String {
        let mut pkgs = vec![] as Vec<Package>;
        self.collect()
            .into_iter()
            .filter(|p| p.is_installed())
            .for_each(|mut p| {
                if p.integrity.is_empty() {
                    p.integrity = p
                        .get_integrity()
                        .expect("Should be able to get package integrity");
                }
                pkgs.push(p);
            });
        serde_json::to_string_pretty(&pkgs).unwrap()
    }

    /// Iterates over all the packages (and their deps) in this config file.
    fn _for_each(pkgs: &mut [Package], mut cb: impl FnMut(&mut Package)) {
        fn inner(pkgs: &mut [Package], cb: &mut impl FnMut(&mut Package)) {
            for p in pkgs {
                cb(p);
                if p.has_deps() {
                    inner(&mut p.dependencies, cb);
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

    #[test]
    fn parse() {
        let _t = crate::test_utils::mktemp();
        let cfgs: [&mut ConfigFile; 3] = [
            &mut ConfigFile::new(&r#"dependencies: { "@bendn/test": 2.0.10 }"#.into()), // quoteless fails as a result of https://github.com/Canop/deser-hjson/issues/9
            &mut ConfigFile::new(&"dependencies:\n  \"@bendn/test\": 2.0.10".into()),
            &mut ConfigFile::new(&"[dependencies]\n\"@bendn/test\" = \"2.0.10\"".into()),
        ];
        #[derive(Debug, Deserialize, Clone, Eq, PartialEq)]
        struct LockFileEntry {
            pub name: String,
            pub version: String,
            pub integrity: String,
        }
        let wanted_lockfile = serde_json::from_str::<Vec<LockFileEntry>>(
            r#"[{"name":"@bendn/test","version":"2.0.10","integrity":"sha512-hyPGxDG8poa2ekmWr1BeTCUa7YaZYfhsN7jcLJ3q2cQVlowcTnzqmz4iV3t21QFyabE5R+rV+y6d5dAItrJeDw=="},{"name":"@bendn/gdcli","version":"1.2.5","integrity":"sha512-/YOAd1+K4JlKvPTmpX8B7VWxGtFrxKq4R0A6u5qOaaVPK6uGsl4dGZaIHpxuqcurEcwPEOabkoShXKZaOXB0lw=="}]"#,
        ).unwrap();
        for cfg in cfgs {
            assert_eq!(cfg.packages.len(), 1);
            assert_eq!(cfg.packages[0].to_string(), "@bendn/test@2.0.10");
            assert_eq!(cfg.packages[0].dependencies.len(), 1);
            assert_eq!(
                cfg.packages[0].dependencies[0].to_string(),
                "@bendn/gdcli@1.2.5"
            );
            cfg.for_each(|p| p.download());
            assert_eq!(
                serde_json::from_str::<Vec<LockFileEntry>>(cfg.lock().as_str()).unwrap(),
                wanted_lockfile
            );
        }
    }
}
