use crate::package::Package;
use core::cmp::Ordering;
use serde::Deserialize;
use serde_json::Error;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone, Eq, PartialEq, Ord, Default)]
/// struct for representing a NPMManifest, produced from https://registry.npmjs.org/name/ver
/// many propertys are discarded, only tarballs and integrity hashes are kept
pub struct NpmManifest {
    pub tarball: String,
    pub integrity: String,
}

impl PartialOrd for NpmManifest {
    fn partial_cmp(&self, _other: &Self) -> Option<std::cmp::Ordering> {
        Some(Ordering::Equal)
    }
}

#[derive(Debug, Default)]
/// struct for representing a package.json file
/// We only care about the dependencies.
pub struct NpmConfig {
    pub dependencies: Vec<Package>,
}

impl NpmConfig {
    /// Make a [NpmConfig] from a json [String].
    /// JSON **must** contain a dependencies field.
    pub fn from_json(json: &String) -> Result<NpmConfig, Error> {
        #[derive(Debug, Deserialize, Default)]
        #[serde(default)]
        struct W {
            dependencies: HashMap<String, String>,
        }
        match serde_json::from_str::<W>(json) {
            Ok(wrap) => Ok(Self::new(
                wrap.dependencies
                    .into_iter()
                    .map(|(package, version)| Package::new(package, version))
                    .collect(),
            )),
            Err(err) => {
                return Result::<NpmConfig, Error>::Err(err);
            }
        }
    }

    /// instances a new [NpmConfig] from a vector of [Package]s
    pub fn new(dependencies: Vec<Package>) -> Self {
        Self { dependencies }
    }
}
