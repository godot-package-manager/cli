use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use regex::{Captures, Regex};
use reqwest_middleware::ClientWithMiddleware;
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

const REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd, Default, Serialize, Debug)]
/// The package struct.
/// This struct is the powerhouse of the entire system, and manages
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

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd, Default, Debug, Serialize)]
pub struct Manifest {
    #[serde(skip)]
    pub shasum: String,
    pub tarball: String,
    #[serde(skip)]
    pub dependencies: Vec<Package>,
    #[serde(skip)]
    version: Version,
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

impl Package {
    #[inline]
    /// Does this package have dependencies?
    pub fn has_deps(&mut self) -> bool {
        !self.manifest.dependencies.is_empty()
    }

    /// Creates a new [Package] from a name and version.
    /// Makes network calls to get the manifest (which makes network calls to get dependency manifests)
    pub async fn new(name: String, version: String, client: ClientWithMiddleware) -> Result<Self> {
        let version = version.trim();
        if version.is_empty() {
            return Self::new_no_version(name, client).await;
        }
        println!("parsing package {name} {version}");
        let r = Range::new(version)
            .parse()
            .with_context(|| format!("parsing version range {version} for {name}"))?; // this does ~ and ^  and >= and < and || e.q parsing
        let packument = Self::get_packument(client.clone(), name.clone())
            .await
            .context(format!("getting packument for {name}"))?;
        let mut versions = Vec::with_capacity(packument.versions.len());
        for v in packument.versions {
            versions.push(v.version.clone());
            let version = Version::parse(&v.version, None)
                .with_context(|| format!("parsing version from packument of {name}"))?;
            let vlone = v.clone();
            if r.test(&version) {
                return Ok(Self {
                    _lockfile_version_string: v.version.to_string(),
                    version,
                    manifest: v.into_manifest(client).await.with_context(|| {
                        format!("parsing {vlone:?} into Manifest for package {name}")
                    })?,
                    name,
                    ..Default::default()
                });
            };
        }

        return Err(anyhow!("Failed to match version for package {name} matching {version}. Tried versions: {versions:?}"));
    }

    /// Create a package from a [str]. see also [ParsedPackage].
    #[allow(dead_code)] // used for tests
    pub async fn create_from_str(s: &str, client: ClientWithMiddleware) -> Result<Package> {
        ParsedPackage::from_str(s)
            .unwrap()
            .into_package(client)
            .await
    }

    /// Creates a new [Package] from a name, gets the latest version from registry/name.
    pub async fn new_no_version(name: String, client: ClientWithMiddleware) -> Result<Package> {
        let resp = abbreviated_get!(format!("{REGISTRY}/{name}/latest"), client.clone())?
            .text()
            .await?;
        if resp == "\"Not Found\"" {
            return Err(anyhow!("Package {name} was not found"));
        };
        let resp = serde_json::from_str::<ParsedManifest>(&resp)?
            .into_manifest(client.clone())
            .await?;
        Ok(Package {
            name,
            _lockfile_version_string: resp.version.to_string(),
            version: resp.version.clone(),
            manifest: resp,
            ..Default::default()
        })
    }

    /// Returns wether this package is installed.
    pub fn is_installed(&self) -> bool {
        Path::new(&self.download_dir()).exists()
    }

    /// Deletes this [Package].
    pub fn purge(&self) {
        if self.is_installed() {
            remove_dir_all(self.download_dir()).expect("Should be able to remove download dir");
        }
    }

    /// Installs this [Package] to a download directory,
    /// depending on wether this package is a direct dependency or not.
    pub async fn download(&mut self, client: ClientWithMiddleware) {
        self.purge();
        let bytes = get!(&self.manifest.tarball, client)
            .expect("Tarball download should work")
            .bytes()
            .await
            .unwrap()
            .to_vec();

        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        const ERR: &str = "Tarball shasum should be a valid hex string";
        assert_eq!(
            &self.manifest.shasum,
            &format!("{:x}", hasher.finalize()),
            "Tarball did not match checksum!"
        );

        /// Emulates `tar xzf archive --strip-components=1 --directory=P`.
        pub fn unpack<R>(mut archive: Archive<R>, dst: &Path) -> io::Result<()>
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

        unpack(
            Archive::new(GzDecoder::new(&bytes[..])),
            Path::new(&self.download_dir()),
        )
        .expect("Tarball should unpack");
    }

    pub async fn get_packument(client: ClientWithMiddleware, name: String) -> Result<Packument> {
        let resp = abbreviated_get!(&format!("{REGISTRY}/{name}"), client.clone())?
            .text()
            .await
            .with_context(|| format!("getting packument from {REGISTRY}/{name}"))?;
        if resp == "\"Not Found\"" {
            return Err(anyhow!("Package {name} was not found",));
        };
        Ok(serde_json::from_str::<ParsedPackument>(&resp)
            .with_context(|| format!("parsing packument from {REGISTRY}/{name}"))?
            .into())
    }

    /// Returns the download directory for this package depending on wether it is indirect or not.
    pub fn download_dir(&self) -> String {
        if self.indirect {
            self.indirect_download_dir()
        } else {
            self.direct_download_dir()
        }
    }

    /// The download directory if this package is a direct dep.
    fn direct_download_dir(&self) -> String {
        format!("./addons/{}", self.name)
    }

    /// The download directory if this package is a indirect dep.
    fn indirect_download_dir(&self) -> String {
        format!("./addons/__gpm_deps/{}/{}", self.name, self.version)
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
    fn modify_script_loads(
        &self,
        t: &str,
        cwd: &Path,
        dep_map: &HashMap<String, String>,
    ) -> String {
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
    /// godot will automatically re-absolute-ify the path, but that is fine.
    fn modify_tres_loads(
        &self,
        t: &String,
        cwd: &Path,
        dep_map: &HashMap<String, String>,
    ) -> String {
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
    fn modify_load(&self, path: &Path, cwd: &Path, dep_map: &HashMap<String, String>) -> PathBuf {
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
    fn recursive_modify(&self, dir: PathBuf, dep_map: &HashMap<String, String>) -> Result<()> {
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

    fn dep_map(&mut self) -> Result<HashMap<String, String>> {
        let mut dep_map = HashMap::<String, String>::new();
        fn add(p: &Package, dep_map: &mut HashMap<String, String>) -> Result<()> {
            let d = p
                .download_dir()
                .strip_prefix("./")
                .ok_or(anyhow!("cant strip prefix!"))?
                .to_string();
            dep_map.insert(p.name.clone(), d.clone());
            // unscoped (@ben/cli => cli) (for compat)
            if let Some((_, s)) = p.name.split_once('/') {
                dep_map.insert(s.into(), d);
            }
            Ok(())
        }
        for pkg in &self.manifest.dependencies {
            add(pkg, &mut dep_map)?;
        }
        add(self, &mut dep_map)?;
        Ok(dep_map)
    }

    /// The catalyst for `recursive_modify`.
    pub fn modify(&mut self) {
        if !self.is_installed() {
            panic!("Attempting to modify a package that is not installed");
        }

        let map = &self.dep_map().unwrap();
        self.recursive_modify(Path::new(&self.download_dir()).to_path_buf(), map)
            .unwrap();
    }
}

impl fmt::Display for Package {
    /// Stringifies this [Package], format my_p@1.0.0.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[cfg(test)]
mod tests {
    use crate::package::*;

    #[tokio::test]
    async fn download() {
        let _t = crate::test_utils::mktemp();
        let c = crate::mkclient();
        let mut p = Package::create_from_str("@bendn/test:2.0.10", c.clone())
            .await
            .unwrap();
        p.download(c.clone()).await;
        assert_eq!(
            crate::test_utils::hashd(p.download_dir().as_str()),
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
            Package::create_from_str("@bendn/test@2.0.10", crate::mkclient())
                .await
                .unwrap()
                .dep_map()
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
        let _t = crate::test_utils::mktemp();
        let c = crate::mkclient();
        let mut p = Package::create_from_str("@bendn/test=2.0.10", c.clone())
            .await
            .unwrap();
        let dep_map = &p.dep_map().unwrap();
        let cwd = Path::new("addons/@bendn/test").into();
        p.download(c).await;
        p.indirect = false;
        assert_eq!(
            p.modify_load(Path::new("addons/test/main.gd"), cwd, dep_map)
                .to_str()
                .unwrap(),
            "addons/@bendn/test/main.gd"
        );

        // dependency usage test
        assert_eq!(
            p.modify_load(Path::new("addons/gdcli/Parser.gd"), cwd, dep_map)
                .to_str()
                .unwrap(),
            "addons/__gpm_deps/@bendn/gdcli/1.2.5/Parser.gd"
        )
    }
}
