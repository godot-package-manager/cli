use crate::config_file::ConfigFile;
use anyhow::{anyhow, Result};
use async_recursion::async_recursion;
use flate2::read::GzDecoder;
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use serde_json::Value as JValue;
use sha1::{Digest, Sha1};
use std::fs::{create_dir_all, read_dir, read_to_string, remove_dir_all, write};
use std::io;
use std::path::{Component::Normal, Path, PathBuf};
use std::str::FromStr;
use std::{collections::HashMap, fmt};
use tar::{Archive, EntryType::Directory};

const REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd, Default, Serialize, Debug)]
/// The package struct.
/// This struct is the powerhouse of the entire system, and manages
/// - installation
/// - modification (of the loads, so they load the right stuff)
/// - removal
pub struct Package {
    pub name: String,
    pub version: String,
    #[serde(skip)]
    pub dependencies: Vec<Package>,
    #[serde(skip)]
    pub indirect: bool,
    #[serde(flatten)]
    pub manifest: Option<Manifest>,
}
#[derive(Default, Clone, Debug)]
pub struct ParsedPackage {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd, Default, Debug, Serialize)]
pub struct Manifest {
    pub integrity: String,
    #[serde(skip_serializing)]
    pub shasum: String,
    #[serde(skip_serializing)]
    pub tarball: String,
}

impl ParsedPackage {
    /// Turn into a [Package].
    pub async fn into_package(self) -> Result<Package> {
        if self.version.is_some() {
            Package::new(self.name, self.version.unwrap()).await
        } else {
            Package::new_no_version(self.name).await
        }
    }
}

impl fmt::Display for ParsedPackage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}@{}",
            self.name,
            self.version.as_ref().unwrap_or(&"latest".to_string())
        )
    }
}

impl FromStr for ParsedPackage {
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
                return Ok(ParsedPackage {name: s.to_string(), ..Default::default()});
            };
            check(p)?;
            Ok(ParsedPackage {
                name: p.to_string(),
                version: Some(v.to_string()),
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
                    return Ok(ParsedPackage {name: s.to_string(), ..Default::default()});
                };
                check(&format!("@{p}")[..])?;
                return Ok(ParsedPackage {
                    name: format!("@{p}"),
                    version: Some(v.to_string()),
                });
            }
            return split_p(s, '@');
        };
    }
}

impl Package {
    #[inline]
    /// Does this package have dependencies?
    pub fn has_deps(&self) -> bool {
        !self.dependencies.is_empty()
    }

    /// Creates a new [Package] from a name and version.
    /// Calls the Package::get_deps() function, so it will
    /// try to access the fs, and if it fails, it will make
    /// calls to cdn.jsdelivr.net to get the `package.json` file.
    pub async fn new(name: String, version: String) -> Result<Package> {
        let mut p = Package {
            name,
            version,
            ..Default::default()
        };
        p.get_deps().await?;
        Ok(p)
    }

    /// Create a package from a [str]. see also [ParsedPackage].
    #[allow(dead_code)] // used for tests
    pub async fn create_from_str(s: &str) -> Result<Package> {
        ParsedPackage::from_str(s).unwrap().into_package().await
    }

    /// Creates a new [Package] from a name, gets the latest version from registry/name.
    pub async fn new_no_version(name: String) -> Result<Package> {
        let resp = reqwest::get(&format!("{REGISTRY}/{name}"))
            .await?
            .text()
            .await?;
        if resp == "\"Not Found\"" {
            return Err(anyhow!("Package {name} was not found"));
        };
        let resp = serde_json::from_str::<JValue>(&resp)?;
        let v = resp
            .get("dist-tags")
            .ok_or(anyhow!("No dist tags!"))?
            .get("latest")
            .ok_or(anyhow!("No latest!"))?
            .as_str()
            .ok_or(anyhow!("Latest not string!"))?;
        let mut p = Package::new(name, v.to_string()).await?;
        p.manifest = serde_json::from_str(
            resp.get("versions")
                .ok_or(anyhow!("No versions!"))?
                .get(v)
                .ok_or(anyhow!("No latest version!"))?
                .to_string()
                .as_str(),
        )
        .expect("Manifest");
        p.get_deps().await?;
        Ok(p)
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
    pub async fn download(&mut self) {
        self.purge();
        let bytes = reqwest::get(&self.get_manifest().await.unwrap().tarball)
            .await
            .expect("Tarball download should work")
            .bytes()
            .await
            .unwrap()
            .to_vec();

        let mut hasher = Sha1::new();
        hasher.update(&bytes);
        const ERR: &str = "Tarball shasum should be a valid hex string";
        assert_eq!(
            self.get_manifest().await.unwrap().shasum,
            format!("{:x}", hasher.finalize()),
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

    /// Gets the [ConfigFile] for this [Package].
    /// Will attempt to read the `package.json` file, if this package is installed.
    /// Else it will make network calls to `cdn.jsdelivr.net`.
    #[async_recursion]
    pub async fn get_config_file(&self) -> Result<ConfigFile> {
        fn get(f: String) -> io::Result<String> {
            read_to_string(Path::new(&f).join("package.json"))
        }
        #[rustfmt::skip]
        let c: Option<String> = if let Ok(c) = get(self.indirect_download_dir()) { Some(c) }
                                else if let Ok(c) = get(self.download_dir()) { Some(c) }
                                else { None };
        if let Some(c) = c {
            if let Ok(n) = ConfigFile::parse(&c, crate::config_file::ConfigType::JSON).await {
                return Ok(n);
            }
        }
        ConfigFile::parse(
            &reqwest::get(&format!(
                "https://cdn.jsdelivr.net/npm/{}@{}/package.json",
                self.name, self.version,
            ))
            .await
            .map_err(|_| {
                anyhow!("Request to cdn.jsdelivr.net failed, package/version doesnt exist")
            })?
            .text()
            .await?,
            crate::config_file::ConfigType::JSON,
        )
        .await
    }

    /// Gets the package manifest and puts it in `self.manfiest`.
    pub async fn get_manifest(&mut self) -> Result<&Manifest> {
        if self.manifest.is_some() {
            return Ok(self.manifest.as_ref().unwrap());
        }
        let resp = reqwest::get(&format!("{REGISTRY}/{}/{}", self.name, self.version))
            .await?
            .text()
            .await?;
        if resp == "\"Not Found\"" {
            return Err(anyhow!(
                "Package {}@{} was not found",
                self.name,
                self.version
            ));
        } else if resp == format!("\"version not found: {}\"", self.version) {
            return Err(anyhow!(
                "Package {} exists, but version '{}' not found",
                self.name,
                self.version
            ));
        }
        #[derive(Deserialize)]
        struct W {
            dist: Manifest,
        }
        let manifest = serde_json::from_str::<W>(resp.as_str())
            .unwrap_or_else(|_| panic!("Unable to get manifest for package {self}"))
            .dist;
        self.manifest = Some(manifest);
        return Ok(self.manifest.as_ref().unwrap());
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

    /// Gets the dependencies of this [Package], placing them in `self.dependencies`.
    async fn get_deps(&mut self) -> Result<()> {
        let cfg = self.get_config_file().await?;
        cfg.packages.into_iter().for_each(|mut dep| {
            dep.indirect = true;
            self.dependencies.push(dep);
        });
        Ok(())
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
    fn recursive_modify(&self, dir: PathBuf, dep_map: &HashMap<String, String>) -> io::Result<()> {
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

    fn dep_map(&self) -> HashMap<String, String> {
        let mut dep_map = HashMap::<String, String>::new();
        fn add(p: &Package, dep_map: &mut HashMap<String, String>) {
            let d = p.download_dir().strip_prefix("./").unwrap().to_string();
            dep_map.insert(p.name.clone(), d.clone());
            // unscoped (@ben/cli => cli) (for compat)
            if let Some((_, s)) = p.name.split_once('/') {
                dep_map.insert(s.into(), d);
            }
        }
        for pkg in &self.dependencies {
            add(pkg, &mut dep_map);
        }
        add(self, &mut dep_map);
        dep_map
    }

    /// The catalyst for `recursive_modify`.
    pub fn modify(&self) -> io::Result<()> {
        if !self.is_installed() {
            panic!("Attempting to modify a package that is not installed");
        }

        self.recursive_modify(
            Path::new(&self.download_dir()).to_path_buf(),
            &self.dep_map(),
        )
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
        let mut p = Package::create_from_str("@bendn/test:2.0.10")
            .await
            .unwrap();
        p.download().await;
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
            Package::create_from_str("@bendn/test@2.0.10")
                .await
                .unwrap()
                .dep_map(),
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
        let mut p = Package::create_from_str("@bendn/test=2.0.10")
            .await
            .unwrap();
        let dep_map = &p.dep_map();
        let cwd = Path::new("addons/@bendn/test").into();
        p.download().await;
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
