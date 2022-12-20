use crate::package::Package;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::write;
use std::path::PathBuf;

#[derive(Debug, Default)]
/// The config file: parsed from godot.package, usually.
/// Contains only a list of [Package]s, currently.
pub struct ConfigFile {
    pub packages: Vec<Package>,
    // hooks: there are no hooks now
}
#[derive(Debug, Serialize)]
/// A package lock object. Goes into the godot.lock file.
struct PackageLock {
    version: String,
    integrity: String,
}

impl ConfigFile {
    /// Creates a new [ConfigFile] from the given path.
    /// Panics if the file doesn't exist, or the file cant be parsed as toml, hjson or yaml.
    pub fn new(path: PathBuf) -> Self {
        #[derive(Debug, Deserialize, Default)]
        #[serde(default)]
        struct W {
            packages: HashMap<String, String>,
        }
        let contents = &std::fs::read_to_string(path).expect("The config file should exist");
        #[rustfmt::skip]
        let cfg: W = if let Ok(w) = deser_hjson::from_str(contents) { w }
                     else if let Ok(w) = serde_yaml::from_str(contents) { w }
                     else if let Ok(w) = toml::from_str(contents) { w }
                     else { panic!("Failed to parse the config file") };
        let mut cfg_file = ConfigFile::default();
        cfg_file.packages = cfg
            .packages
            .into_iter()
            .map(|(name, version)| Package::new(name, version))
            .collect();
        cfg_file.packages.sort();
        cfg_file
    }

    /// Creates a lockfile for this config file.
    /// note: Lockfiles are currently unused.
    pub fn lock(&mut self) {
        write(
            "./godot.lock",
            serde_json::to_string(
                &self
                    .collect()
                    .into_iter()
                    .filter_map(|p| {
                        p.is_installed()
                            .then_some((p.name.clone(), PackageLock::new(p)))
                    })
                    .collect::<HashMap<String, PackageLock>>(),
            )
            .unwrap(),
        )
        .expect("Writing lock file should work");
    }

    /// Iterates over all the packages (and their deps) in this config file.
    fn _for_each(pkgs: &mut [Package], mut cb: impl FnMut(&mut Package)) {
        fn inner(pkgs: &mut [Package], cb: &mut impl FnMut(&mut Package)) {
            for p in pkgs {
                cb(p);
                if p.has_deps() {
                    inner(&mut p.meta.dependencies, cb);
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

impl PackageLock {
    /// Create a new [PackageLock] from a [Package], so it can be serialized easily.
    /// Theres probably a way to make serialization of [Package] just ignore certain fields,
    /// so this wouldn't be necessary.
    fn new(mut pkg: Package) -> Self {
        if pkg.meta.npm_manifest.integrity.is_empty() {
            pkg.get_manifest()
        };
        Self {
            version: pkg.version,
            integrity: pkg.meta.npm_manifest.integrity,
        }
    }
}
