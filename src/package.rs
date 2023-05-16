use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use regex::{Captures, Regex};
use reqwest::Client;
use semver_rs::{Parseable, Range, Version};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::fs::{create_dir_all, read_dir, read_to_string, remove_dir_all, write};
use std::io;
use std::path::{Component::Normal, Path, PathBuf};
use std::str::FromStr;
use std::{collections::HashMap, fmt};
use tar::{Archive, EntryType::Directory};

pub mod parsing;
use parsing::*;

use crate::config_file::Cache;

const REGISTRY: &str = "https://registry.npmjs.org";

type DepMap = HashMap<String, PathBuf>;

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd, Default, Serialize, Hash)]
/// The package struct.
/// This struct powers the entire system, and manages
/// - installation
/// - modification (of the loads, so they load the right stuff)
/// - removal
pub struct Package {
    pub name: String,
    #[serde(skip)]
    pub version: Version,
    #[serde(skip)]
    pub indirect: bool,
    #[serde(flatten)]
    pub manifest: Manifest,
    #[serde(rename = "version")]
    pub _lockfile_version_string: String, // for lockfile, do not use
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd, Default, Debug, Serialize, Hash)]
pub struct Manifest {
    #[serde(skip)]
    pub shasum: String,
    pub tarball: String,
    #[serde(skip)]
    pub dependencies: Vec<Package>,
    #[serde(skip)]
    version: Version,
}

#[macro_export]
macro_rules! ctx {
    ($e:expr, $fmt:literal $(, $args:expr)* $(,)?) => {
        $e.with_context(||format!($fmt $(, $args)*))
    };
}

macro_rules! abbreviated_get {
    ($url: expr, $client: expr) => {
        $client
            .get($url)
            .header(
                "Accept",
                "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8",
            )
            .send()
            .await
    };
}

macro_rules! get {
    ($url: expr, $client: expr) => {
        $client.get($url).send().await
    };
}

/// Emulates `tar xzf archive --strip-components=1 --directory=P`.
fn unpack<R>(mut archive: Archive<R>, dst: &Path) -> io::Result<()>
where
    R: io::Read,
{
    if dst.symlink_metadata().is_err() {
        create_dir_all(dst)?;
    }

    let dst = &dst.canonicalize().unwrap_or(dst.to_path_buf());

    // Delay any directory entries until the end (they will be created if needed by
    // descendants), to ensure that directory permissions do not interfer with descendant
    // extraction.
    let mut directories = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        let mut entry = (
            dst.join(
                entry
                    .path()?
                    .components()
                    .skip(1)
                    .filter(|c| matches!(c, Normal(_)))
                    .collect::<PathBuf>(),
            ),
            entry,
        );
        if entry.1.header().entry_type() == Directory {
            directories.push(entry);
        } else {
            create_dir_all(entry.0.parent().unwrap())?;
            entry.1.unpack(entry.0)?;
        }
    }
    for mut dir in directories {
        dir.1.unpack(dir.0)?;
    }
    Ok(())
}

impl Package {
    #[inline]
    /// Does this package have dependencies?
    pub fn has_deps(&mut self) -> bool {
        !self.manifest.dependencies.is_empty()
    }

    /// Creates a new [Package] from a name and version.
    /// Makes network calls to get the manifest (which makes network calls to get dependency manifests) (unless cached)
    pub async fn new(name: String, version: String, client: Client, cache: Cache) -> Result<Self> {
        let version = version.trim();
        if version.is_empty() {
            // i forgot what this is for
            return Self::new_no_version(name, client, cache).await;
        }
        let r = ctx!(
            Range::new(version).parse(),
            "parsing version range {version} for {name}"
        )?; // this does ~ and ^  and >= and < and || e.q parsing
        if let Some(versions) = cache.lock().unwrap().get(&name) {
            for (_, package) in versions {
                if r.test(&package.version) {
                    return Ok(package.clone());
                }
            }
        }
        let packument = ctx!(
            Self::get_packument(client.clone(), &name).await,
            "getting packument for {name}"
        )?;
        let mut versions = Vec::with_capacity(packument.versions.len());
        for v in packument.versions {
            versions.push(v.version.clone());
            let version = ctx!(
                Version::parse(&v.version, None),
                "parsing version from packument of {name}"
            )?;
            let vlone = v.clone(); // purely for the print
            if r.test(&version) {
                let p = Package {
                    _lockfile_version_string: v.version.to_string(),
                    version,
                    manifest: ctx!(
                        v.clone().into_manifest(client, cache.clone()).await,
                        "parsing {vlone:?} into Manifest for package {name}"
                    )?,
                    name: name.clone(),
                    ..Default::default()
                };
                cache
                    .lock()
                    .unwrap()
                    .entry(name)
                    .or_default()
                    .insert(v.version.to_string(), p.clone());
                return Ok(p);
            };
        }

        return Err(anyhow!("Failed to match version for package {name} matching {version}. Tried versions: {versions:?}"));
    }

    /// Create a package from a [str]. see also [ParsedPackage].
    #[allow(dead_code)] // used for tests
    pub async fn create_from_str(s: &str, client: Client, cache: Cache) -> Result<Package> {
        ParsedPackage::from_str(s)
            .unwrap()
            .into_package(client, cache)
            .await
    }

    /// Creates a new [Package] from a name, gets the latest version from registry/name.
    pub async fn new_no_version(name: String, client: Client, cache: Cache) -> Result<Package> {
        const MARKER: &str = "🐢"; // latest
        if let Some(versions) = cache.lock().unwrap().get_mut(&name) {
            if let Some(marker) = versions.get(MARKER) {
                return Ok(marker.clone());
            }
        }
        let resp = abbreviated_get!(format!("{REGISTRY}/{name}/latest"), client.clone())?
            .text()
            .await?;
        if resp == "\"Not Found\"" {
            return Err(anyhow!("Package {name} was not found"));
        };
        let resp = serde_json::from_str::<ParsedManifest>(&resp)?
            .into_manifest(client.clone(), cache.clone())
            .await?;
        let latest = Package {
            name: name.to_owned(),
            _lockfile_version_string: resp.version.to_string(),
            version: resp.version.clone(),
            manifest: resp,
            ..Default::default()
        };
        cache
            .lock()
            .unwrap()
            .entry(name)
            .or_default()
            .insert(MARKER.to_owned(), latest.clone());
        Ok(latest)
    }

    /// Returns wether this package is installed.
    pub fn is_installed(&self, cwd: &Path) -> bool {
        self.download_dir(cwd).exists()
    }

    /// Deletes this [Package].
    pub fn purge(&self, cwd: &Path) {
        if self.is_installed(cwd) {
            remove_dir_all(self.download_dir(cwd)).expect("Should be able to remove download dir");
        }
    }

    /// Installs this [Package] to a download directory,
    /// depending on wether this package is a direct dependency or not.
    pub async fn download(&mut self, client: Client, cwd: &Path) {
        self.purge(cwd);
        let bytes = get!(&self.manifest.tarball, client)
            .expect("Tarball download should work")
            .bytes()
            .await
            .unwrap()
            .to_vec();

        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        assert_eq!(
            &self.manifest.shasum,
            &format!("{:x}", hasher.finalize()),
            "Tarball did not match checksum!"
        );

        unpack(
            Archive::new(GzDecoder::new(&bytes[..])),
            &self.download_dir(cwd),
        )
        .expect("Tarball should unpack");
    }

    pub async fn get_packument(client: Client, name: &str) -> Result<Packument> {
        let resp = abbreviated_get!(&format!("{REGISTRY}/{name}"), client.clone())?
            .text()
            .await
            .with_context(|| format!("getting packument from {REGISTRY}/{name}"))?;
        if resp == "\"Not Found\"" {
            return Err(anyhow!("Package {name} was not found",));
        };
        Ok(ctx!(
            serde_json::from_str::<ParsedPackument>(&resp),
            "parsing packument from {REGISTRY}/{name}"
        )?
        .into())
    }

    /// Returns the download directory for this package depending on wether it is indirect or not.
    pub fn download_dir(&self, cwd: &Path) -> PathBuf {
        if self.indirect {
            self.indirect_download_dir(cwd)
        } else {
            self.direct_download_dir(cwd)
        }
    }

    /// The download directory if this package is a direct dep.
    fn direct_download_dir(&self, cwd: &Path) -> PathBuf {
        cwd.join("addons").join(self.name.clone())
    }

    /// The download directory if this package is a indirect dep.
    fn indirect_download_dir(&self, cwd: &Path) -> PathBuf {
        cwd.join("addons")
            .join("__gpm_deps")
            .join(self.name.clone())
            .join(self.version.to_string())
    }
}

// package modification block
impl Package {
    /// Modifies the loads of a GDScript script.
    /// ```gdscript
    /// extends Node
    ///
    /// const Wow = preload("res://addons/my_awesome_addon/wow.gd")
    /// ```
    /// =>
    /// ```gdscript
    /// # --snip--
    /// const Wow = preload("res://addons/__gpm_deps/my_awesome_addon/wow.gd")
    /// ```
    fn modify_script_loads(&self, t: &str, cwd: &Path, dep_map: &DepMap) -> String {
        lazy_static::lazy_static! {
            static ref SCRIPT_LOAD_R: Regex = Regex::new("(pre)?load\\([\"']([^)]+)['\"]\\)").unwrap();
        }
        SCRIPT_LOAD_R
            .replace_all(t, |c: &Captures| {
                let p = Path::new(c.get(2).unwrap().as_str());
                let res = self.modify_load(p.strip_prefix("res://").unwrap_or(p), cwd, dep_map);
                let preloaded = if c.get(1).is_some() { "pre" } else { "" };
                if res == p {
                    format!("{preloaded}load('{}')", p.display())
                } else {
                    format!("{preloaded}load('res://{}')", res.display())
                }
            })
            .to_string()
    }

    /// Modifies the loads of a godot TextResource.
    /// ```gdresource
    /// [gd_scene load_steps=1 format=2]
    ///
    /// [ext_resource path="res://addons/my_awesome_addon/wow.gd" type="Script" id=1]
    /// ```
    /// =>
    /// ```gdresource
    /// --snip--
    /// [ext_resource path="res://addons/__gpm_deps/my_awesome_addon/wow.gd" type="Script" id=1]
    /// ```
    fn modify_tres_loads(&self, t: &str, cwd: &Path, dep_map: &DepMap) -> String {
        lazy_static::lazy_static! {
            static ref TRES_LOAD_R: Regex = Regex::new(r#"\[ext_resource path="([^"]+)""#).unwrap();
        }
        TRES_LOAD_R
            .replace_all(t, |c: &Captures| {
                let p = Path::new(c.get(1).unwrap().as_str());
                let res = self.modify_load(
                    p.strip_prefix("res://")
                        .expect("TextResource path should be absolute"),
                    cwd,
                    dep_map,
                );
                if res == p {
                    format!(r#"[ext_resource path="{}""#, p.display())
                } else {
                    format!(r#"[ext_resource path="res://{}""#, res.display())
                }
            })
            .to_string()
    }

    /// The backend for modify_script_loads and modify_tres_loads.
    fn modify_load(&self, path: &Path, cwd: &Path, dep_map: &DepMap) -> PathBuf {
        // if it works, skip it
        if path.exists() || cwd.join(path).exists() {
            return path.to_path_buf();
        }
        if let Some(c) = path.components().nth(1) {
            if let Some(addon_dir) = dep_map.get(&String::from(c.as_os_str().to_str().unwrap())) {
                let wanted_f =
                    Path::new(addon_dir).join(path.components().skip(2).collect::<PathBuf>());
                return wanted_f;
            }
        };
        eprintln!(
            "{:>12} Could not find path for {path:#?}",
            crate::putils::warn()
        );
        path.to_path_buf()
    }

    /// Recursively modifies a directory.
    fn recursive_modify(&self, dir: PathBuf, dep_map: &DepMap) -> Result<()> {
        for entry in read_dir(&dir)? {
            let p = entry?;
            if p.path().is_dir() {
                self.recursive_modify(p.path(), dep_map)?;
                continue;
            }

            #[derive(PartialEq, Debug)]
            enum Type {
                TextResource,
                GDScript,
            }
            if let Some(e) = p.path().extension() {
                let t = if e == "tres" || e == "tscn" {
                    Type::TextResource
                } else if e == "gd" || e == "gdscript" {
                    Type::GDScript
                } else {
                    continue;
                };
                let text = read_to_string(p.path())?;
                write(
                    p.path(),
                    match t {
                        Type::TextResource => self.modify_tres_loads(&text, &dir, dep_map),
                        Type::GDScript => self.modify_script_loads(&text, &dir, dep_map),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn dep_map(&mut self, cwd: &Path) -> Result<DepMap> {
        let mut dep_map = HashMap::<String, PathBuf>::new();
        fn add(p: &Package, dep_map: &mut DepMap, cwd: &Path) -> Result<()> {
            let d = p.download_dir(cwd);
            dep_map.insert(p.name.clone(), d.clone());
            // unscoped (@ben/cli => cli) (for compat)
            if let Some((_, s)) = p.name.split_once('/') {
                dep_map.insert(s.into(), d);
            }
            Ok(())
        }
        for pkg in &self.manifest.dependencies {
            add(pkg, &mut dep_map, cwd)?;
        }
        add(self, &mut dep_map, cwd)?;
        Ok(dep_map)
    }

    /// The catalyst for `recursive_modify`.
    pub fn modify(&mut self, cwd: &Path) {
        if !self.is_installed(cwd) {
            panic!("Attempting to modify a package that is not installed");
        }

        let map = &self.dep_map(cwd).unwrap();
        self.recursive_modify(self.download_dir(cwd), map).unwrap();
    }
}

impl fmt::Display for Package {
    /// Stringifies this [Package], format my_p@1.0.0.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

impl fmt::Debug for Package {
    /// Mirrors the [Display] impl.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self, f)
    }
}

#[cfg(test)]
mod tests {
    use crate::{config_file::create_cache, package::*};

    #[tokio::test]
    async fn download() {
        let t = crate::test_utils::mktemp();
        let c = crate::mkclient();
        let mut p = Package::create_from_str("@bendn/test:2.0.10", c.clone(), create_cache())
            .await
            .unwrap();
        p.download(c.clone(), t.path()).await;
        assert_eq!(
            crate::test_utils::hashd(&p.download_dir(t.path())),
            [
                "1c2fd93634817a9e5f3f22427bb6b487520d48cf3cbf33e93614b055bcbd1329", // readme.md
                "c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e", // sub1.gd
                "c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e", // sub2.gd
                "d711b57105906669572a0e53b8b726619e3a21463638aeda54e586a320ed0fc5", // main.gd
                "e4f9df20b366a114759282209ff14560401e316b0059c1746c979f478e363e87", // package.json
            ]
        );
    }

    #[tokio::test]
    async fn dep_map() {
        // no fs was touched in the making of this test
        assert_eq!(
            Package::create_from_str("@bendn/test@2.0.10", crate::mkclient(), create_cache())
                .await
                .unwrap()
                .dep_map(Path::new(""))
                .unwrap(),
            HashMap::from([
                ("test".into(), "addons/@bendn/test".into()),
                ("@bendn/test".into(), "addons/@bendn/test".into()),
                (
                    "@bendn/gdcli".into(),
                    "addons/__gpm_deps/@bendn/gdcli/1.2.5".into()
                ),
                (
                    "gdcli".into(),
                    "addons/__gpm_deps/@bendn/gdcli/1.2.5".into()
                ),
            ])
        );
    }

    #[tokio::test]
    async fn modify_load() {
        let t = crate::test_utils::mktemp();
        let c = crate::mkclient();
        let mut p = Package::create_from_str("@bendn/test=2.0.10", c.clone(), create_cache())
            .await
            .unwrap();
        let dep_map = &p.dep_map(t.path()).unwrap();
        p.download(c, t.path()).await;
        p.indirect = false;
        let cwd = t.path().join("addons/@bendn/test");
        assert_eq!(
            Path::new(
                p.modify_load(Path::new("addons/test/main.gd"), &cwd, dep_map)
                    .to_str()
                    .unwrap()
            ),
            t.path().join("addons/@bendn/test/main.gd")
        );

        // dependency usage test
        assert_eq!(
            Path::new(
                p.modify_load(Path::new("addons/gdcli/Parser.gd"), &cwd, dep_map)
                    .to_str()
                    .unwrap()
            ),
            t.path()
                .join("addons/__gpm_deps/@bendn/gdcli/1.2.5/Parser.gd")
        )
    }
}
