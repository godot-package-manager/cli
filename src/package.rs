use crate::config_file::ConfigFile;
use flate2::read::GzDecoder;
use regex::{Captures, Regex};
use serde::Serialize;
use serde_json::Value as JValue;
use std::fs::{create_dir_all, read_dir, read_to_string, remove_dir_all, write};
use std::io;
use std::path::{Component::Normal, Path, PathBuf};
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
    pub integrity: String, // cant be bothered to use Options
    #[serde(skip)]
    manifest: String,
}

impl Package {
    /// Does this package have dependencies?
    pub fn has_deps(&self) -> bool {
        !self.dependencies.is_empty()
    }

    /// Creates a new [Package] from a name and version.
    /// Calls the Package::get_deps() function, so it will
    /// try to access the fs, and if it fails, it will make
    /// calls to cdn.jsdelivr.net to get the `package.json` file.
    pub fn new(name: String, version: String) -> Package {
        let mut p = Package::default();
        p.name = name;
        p.version = version;
        p.get_deps();
        p
    }

    /// Stringifies this [Package], format my_p@1.0.0.
    pub fn to_string(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Returns wether this package is installed.
    pub fn is_installed(&self) -> bool {
        Path::new(&self.direct_download_dir()).exists()
            || Path::new(&self.indirect_download_dir()).exists()
    }

    /// Deletes this [Package].
    pub fn purge(&self) {
        if self.is_installed() {
            remove_dir_all(self.download_dir()).expect("Should be able to remove download dir");
        }
    }

    /// Installs this [Package] to a download directory,
    /// depending on wether this package is a direct dependency or not.
    pub fn download(&mut self) {
        println!("Downloading {self}");
        self.purge();
        let resp = ureq::get(&self.get_tarball().expect("Should be able to get tarball"))
            .call()
            .expect("Tarball download should work");

        let len = resp
            .header("Content-Length")
            .expect("Tarball should specify content length")
            .parse()
            .expect("Tarball content length should be a number");

        let mut bytes: Vec<u8> = Vec::with_capacity(len);
        resp.into_reader()
            .read_to_end(&mut bytes)
            .expect("Tarball should be bytes");

        /// Emulates `tar xzf archive --strip-components=1 --directory=P`.
        pub fn unpack<R>(mut archive: Archive<R>, dst: &Path) -> io::Result<()>
        where
            R: io::Read,
        {
            if dst.symlink_metadata().is_err() {
                create_dir_all(&dst)?;
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
    pub fn get_config_file(&self) -> ConfigFile {
        fn get(f: String) -> io::Result<String> {
            read_to_string(Path::new(&f).join("package.json"))
        }
        #[rustfmt::skip]
        let c: Option<String> = if let Ok(c) = get(self.indirect_download_dir()) { Some(c) }
                                else if let Ok(c) = get(self.download_dir()) { Some(c) }
                                else { None };
        if let Some(c) = c {
            if let Ok(n) = ConfigFile::from_json(&c) {
                return n;
            }
        }
        ConfigFile::from_json(
            &ureq::get(&format!(
                "https://cdn.jsdelivr.net/npm/{}@{}/package.json",
                self.name, self.version,
            ))
            .call()
            .expect("Getting the package config file should not fail")
            .into_string()
            .expect("The package config file should be valid text"),
        )
        .expect("The package config file should be correct/valid JSON")
    }

    /// Gets the package manifest and puts it in `self.manfiest`.
    fn get_manifest(&mut self) {
        let resp = ureq::get(&format!("{}/{}/{}", REGISTRY, self.name, self.version))
            .call()
            .expect("Getting the package manifest file should not fail")
            .into_string()
            .expect("The package manifest file should be valid text");
        if resp == "\"Not Found\"" {
            panic!("Package {}@{} was not found", self.name, self.version)
        } else if resp == format!("\"version not found: {}\"", self.version) {
            panic!(
                "Package {} exists, but version '{}' not found",
                self.name, self.version
            )
        }
        let _ = serde_json::from_str::<JValue>(&resp).expect("Manifest should be valid JSON");
        self.manifest = resp
    }

    /// Gets the package tarball.
    pub fn get_tarball(&mut self) -> Option<String> {
        if self.manifest.is_empty() {
            self.get_manifest();
        }
        let j = serde_json::from_str::<JValue>(&self.manifest).unwrap();
        Some(j.get("dist")?.get("tarball")?.as_str()?.to_string())
    }

    /// Gets the package integrity.
    pub fn get_integrity(&mut self) -> Option<String> {
        if self.manifest.is_empty() {
            self.get_manifest();
        }
        let j = serde_json::from_str::<JValue>(&self.manifest).unwrap();
        Some(j.get("dist")?.get("integrity")?.as_str()?.to_string())
        // TODO: try and get the integrity manually if already installed
    }

    /// Returns the download directory for this package depending on wether it is indirect or not.
    fn download_dir(&self) -> String {
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
    fn get_deps(&mut self) -> &Vec<Package> {
        let cfg = self.get_config_file();
        cfg.packages.into_iter().for_each(|mut dep| {
            dep.indirect = true;
            self.dependencies.push(dep);
        });
        &self.dependencies
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
        t: &String,
        cwd: &PathBuf,
        dep_map: &HashMap<String, String>,
    ) -> String {
        lazy_static::lazy_static! {
            static ref SCRIPT_LOAD_R: Regex = Regex::new("(pre)?load\\([\"']([^)]+)['\"]\\)").unwrap();
        }
        SCRIPT_LOAD_R
            .replace_all(&t, |c: &Captures| {
                let m = Path::new(c.get(2).unwrap().as_str());
                format!(
                    "{}load('res://{}')",
                    if c.get(1).is_some() { "pre" } else { "" },
                    self.modify_load(m.strip_prefix("res://").unwrap_or(m), cwd, dep_map)
                        .display()
                )
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
        cwd: &PathBuf,
        dep_map: &HashMap<String, String>,
    ) -> String {
        lazy_static::lazy_static! {
            static ref TRES_LOAD_R: Regex = Regex::new("[ext_resource path=\"([^\"]+)\"").unwrap();
        }
        TRES_LOAD_R
            .replace_all(&t, |c: &Captures| {
                format!(
                    r#"[ext_resource path="res://{}""#,
                    self.modify_load(
                        Path::new(c.get(1).unwrap().as_str())
                            .strip_prefix("res://")
                            .expect("TextResource path should be absolute"),
                        cwd,
                        dep_map,
                    )
                    .display()
                )
            })
            .to_string()
    }

    /// The backend for modify_script_loads and modify_tres_loads.
    fn modify_load(
        &self,
        path: &Path,
        cwd: &PathBuf,
        dep_map: &HashMap<String, String>,
    ) -> PathBuf {
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
        eprintln!("Could not find path for {path:#?}");
        return path.to_path_buf();
    }

    /// Recursively modifies a directory.
    fn recursive_modify(
        &self,
        dir: PathBuf,
        deps: &Vec<Package>,
        dep_map: &HashMap<String, String>,
    ) -> io::Result<()> {
        for entry in read_dir(&dir)? {
            let p = entry?;
            if p.path().is_dir() {
                self.recursive_modify(p.path(), deps, dep_map)?;
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
            if let Some((_, s)) = p.name.split_once("/") {
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
    pub fn modify(&self) {
        if self.is_installed() == false {
            panic!("Attempting to modify a package that is not installed");
        }

        if let Err(e) = self.recursive_modify(
            Path::new(&self.download_dir()).to_path_buf(),
            &self.dependencies,
            &self.dep_map(),
        ) {
            println!("Modification of {self} yielded error {e}");
        }
    }
}

impl fmt::Display for Package {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::package::*;

    #[test]
    fn download() {
        let _t = crate::test_utils::mktemp();
        let mut p = Package::new("@bendn/test".into(), "2.0.10".into());
        p.download();
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

    #[test]
    fn dep_map() {
        // no fs was touched in the making of this test
        assert_eq!(
            Package::new("@bendn/test".into(), "2.0.10".into()).dep_map(),
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

    #[test]
    fn modify_load() {
        let _t = crate::test_utils::mktemp();
        let mut p = Package::new("@bendn/test".into(), "2.0.10".into());
        let dep_map = &p.dep_map();
        let cwd = &Path::new("addons/@bendn/test").into(); // holy shit rust is smart -- it knows this needs to be a pathbuf
        p.download();
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
