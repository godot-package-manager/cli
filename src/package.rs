use crate::Cache;
use crate::Client;

use anyhow::bail;
use anyhow::{anyhow, Context, Result};
use async_recursion::async_recursion;
use flate2::read::GzDecoder;
use regex::{Captures, Regex};
use semver_rs::{Range, Version};
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
    pub version: Version,
}

#[macro_export]
macro_rules! ctx {
    ($e:expr, $fmt:literal $(, $args:expr)* $(,)?) => {
        $e.with_context(||format!($fmt $(, $args)*))
    };
}

macro_rules! get {
    ($client: expr, $fmt:literal $(, $args:expr)* $(,)?) => {
        $client.get(&format!($fmt $(, $args)*)).send().await
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
    pub fn from_manifest(m: Manifest, name: String) -> Self {
        Self {
            _lockfile_version_string: m.version.to_string(),
            manifest: m,
            name,
            ..Default::default()
        }
    }

    #[inline]
    /// Does this package have dependencies?
    pub fn has_deps(&mut self) -> bool {
        !self.manifest.dependencies.is_empty()
    }

    /// Creates a new [Package] from a name and version.
    /// Makes network calls to get the manifest (which makes network calls to get dependency manifests) (unless cached)
    #[async_recursion]
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

        if let Some(got) = cache.get_mut(&name) {
            let mut vers = got.clone(); // clone to remove references to dashmap
            drop(got); // drop reference (let x = x doesnt drop original x until scope ends)
            if let Some(mut find) = vers.find_version(&r) {
                // find is a reference to vers which is cloned (not ref to dashmap)
                // this block was supposed to be
                // Ok(find.parse(...).await?.get_package())
                // but then it deadlocked because get_package() would recurse
                find.parse(client, cache.clone(), name.clone()).await?;
                let p = find.get_package();
                cache.insert(name, find.key().clone(), std::mem::take(find.value_mut())); // cloned find, must now replace
                return Ok(p);
            };
        }
        let packument = ctx!(
            Self::get_packument(client.clone(), &name).await,
            "getting packument for {name}"
        )?;
        let mut versions = {
            let ec = cache.clone();
            let mut e = ec.entry(name.clone()).or_default();
            // clone to not have references to dashmap which causes deadlock
            // this does (should) still insert the packument in the real cache
            e.insert_packument(packument).clone()
        };
        // do it again with the new entrys inserted
        if let Some(mut find) = versions.find_version(&r) {
            find.parse(client, cache.clone(), name.clone()).await?;
            let p = find.get_package();
            cache.insert(name, find.key().clone(), std::mem::take(find.value_mut()));
            return Ok(p);
        }
        bail!(
            "Failed to match version for package {name} matching {version}. Tried versions: {:?}",
            versions
        );
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
        if let Some(n) = cache.get(&name) {
            if let Some(marker) = n.get(MARKER) {
                return Ok(marker.get_package()); // doesnt recurse
            }
        }
        let resp = get!(client.clone(), "{}/{name}/latest", client.registry)?
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
            manifest: resp,
            ..Default::default()
        };
        cache.insert(name, MARKER.to_owned(), latest.clone().into());
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
        let bytes = get!(client.clone(), "{}", &self.manifest.tarball)
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
        // println!(
        //     "(\"{}\", hex::decode(\"{}\").unwrap()),",
        //     self.manifest.tarball.replace(&(client.registry + "/"), ""),
        //     hex::encode(&bytes)
        // );
        unpack(
            Archive::new(GzDecoder::new(&bytes[..])),
            &self.download_dir(cwd),
        )
        .expect("Tarball should unpack");
    }

    pub async fn get_packument(client: Client, name: &str) -> Result<Packument> {
        let resp = ctx!(
            get!(client.clone(), "{}/{name}", client.registry)?
                .text()
                .await,
            "getting packument from {}/{name}",
            client.registry
        )?;
        if resp == "\"Not Found\"" {
            return Err(anyhow!("Package {name} was not found",));
        };
        let res = ctx!(
            serde_json::from_str::<ParsedPackument>(&resp),
            "parsing packument from {}/{name}",
            client.registry
        )?;
        // println!(
        //     "(\"{name}\", r#\"{}\"#),",
        //     serde_json::to_string(&res)
        //         .unwrap()
        //         .replace("https://registry.npmjs.org", "{REGISTRY}")
        // );
        Ok(res.into())
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
            .join(self.manifest.version.to_string())
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
        write!(f, "{}@{}", self.name, self.manifest.version)
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
    use crate::{package::*, Cache};

    #[tokio::test]
    async fn download() {
        let t = crate::test_utils::mktemp().await;
        let c = t.2;
        let mut p = Package::create_from_str("@bendn/test:2.0.10", c.clone(), Cache::new())
            .await
            .unwrap();
        p.download(c.clone(), t.0.path()).await;
        assert_eq!(
            crate::test_utils::hashd(&p.download_dir(t.0.path())),
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
            Package::create_from_str(
                "@bendn/test@2.0.10",
                crate::test_utils::mktemp().await.2,
                Cache::new()
            )
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
        let t = crate::test_utils::mktemp().await;
        let c = t.2;
        let mut p = Package::create_from_str("@bendn/test=2.0.10", c.clone(), Cache::new())
            .await
            .unwrap();
        let dep_map = &p.dep_map(t.0.path()).unwrap();
        p.download(c, t.0.path()).await;
        p.indirect = false;
        let cwd = t.0.path().join("addons/@bendn/test");
        assert_eq!(
            Path::new(
                p.modify_load(Path::new("addons/test/main.gd"), &cwd, dep_map)
                    .to_str()
                    .unwrap()
            ),
            t.0.path().join("addons/@bendn/test/main.gd")
        );

        // dependency usage test
        assert_eq!(
            Path::new(
                p.modify_load(Path::new("addons/gdcli/Parser.gd"), &cwd, dep_map)
                    .to_str()
                    .unwrap()
            ),
            t.0.path()
                .join("addons/__gpm_deps/@bendn/gdcli/1.2.5/Parser.gd")
        )
    }
}
