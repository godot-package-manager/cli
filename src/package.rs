use crate::npm::*;
use core::cmp::Ordering;
use flate2::read::GzDecoder;
use regex::{Captures, Regex};
use serde::Deserialize;
use std::fs::{create_dir_all, read_dir, read_to_string, remove_dir_all, write};
use std::io;
use std::path::{Component::Normal, Path, PathBuf};
use std::{collections::HashMap, fmt};
use tar::Archive;

const REGISTRY: &str = "https://registry.npmjs.org";

#[derive(Clone, Eq, PartialEq, Ord)]
/// The package struct.
/// This struct is the powerhouse of the entire system, and manages
/// - installation
/// - modification (of the loads, so they load the right stuff)
/// - removal
pub struct Package {
    pub name: String,
    pub version: String,
    pub meta: PackageMeta,
}

#[derive(Clone, Eq, PartialEq, Ord, Default)]
/// The metadata of a [Package].
/// Stores dependency data.
pub struct PackageMeta {
    pub npm_manifest: NpmManifest,
    pub dependencies: Vec<Package>,
    pub indirect: bool,
}

impl PartialOrd for Package {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        return Some(self.name.cmp(&other.name));
    }
}
impl PartialOrd for PackageMeta {
    fn partial_cmp(&self, _other: &Self) -> Option<Ordering> {
        return Some(Ordering::Equal);
    }
}

impl Package {
    /// Does this package have dependencies?
    pub fn has_deps(&self) -> bool {
        !self.meta.dependencies.is_empty()
    }

    /// Creates a new [Package] from a name and version.
    /// Calls the Package::get_deps() function, so it will
    /// try to access the fs, and if it fails, it will make
    /// calls to cdn.jsdelivr.net to get the `package.json` file.
    pub fn new(name: String, version: String) -> Package {
        let mut p = Package {
            meta: PackageMeta::default(),
            name,
            version,
        };
        p.get_deps();
        p
    }

    /// Stringifies this [Package], format my_p@1.0.0.
    pub fn to_string(&self) -> String {
        format!("{}@{}", self.name, self.version)
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
    pub fn download(&mut self) {
        println!("Downloading {self}");
        self.purge();
        if self.meta.npm_manifest.tarball.is_empty() {
            self.get_manifest()
        };
        let resp = ureq::get(&self.meta.npm_manifest.tarball)
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
        pub fn unpack<P, R>(mut archive: Archive<R>, dst: P) -> io::Result<()>
        where
            P: AsRef<Path>,
            R: io::Read,
        {
            if dst.as_ref().symlink_metadata().is_err() {
                create_dir_all(&dst)?;
            }

            for entry in archive.entries()? {
                let mut entry = entry?;
                let path: PathBuf = entry
                    .path()?
                    .components()
                    .skip(1) // strip top-level directory
                    .filter(|c| matches!(c, Normal(_))) // prevent traversal attacks
                    .collect();
                entry.unpack(dst.as_ref().join(path))?;
            }
            Ok(())
        }

        unpack(
            Archive::new(GzDecoder::new(&bytes[..])),
            Path::new(&self.download_dir()),
        )
        .expect("Tarball should unpack");

        self.modify();
    }

    /// Gets the [NpmConfig] for this [Package].
    /// Will attempt to read the `package.json` file, if this package is installed.
    /// Else it will make network calls to `cdn.jsdelivr.net`.
    pub fn get_config_file(&self) -> NpmConfig {
        fn get(f: String) -> io::Result<String> {
            read_to_string(Path::new(&f).join("package.json"))
        }
        #[rustfmt::skip]
        let c: Option<String> = if let Ok(c) = get(self.indirect_download_dir()) { Some(c) }
                                else if let Ok(c) = get(self.download_dir()) { Some(c) }
                                else { None };
        if let Some(c) = c {
            if let Ok(n) = NpmConfig::from_json(&c) {
                return n;
            }
        }
        NpmConfig::from_json(
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

    /// Gets the [NpmManifest], and puts it in `self.meta.npm_manifest`.
    pub fn get_manifest(&mut self) {
        #[derive(Debug, Deserialize)]
        struct W {
            pub dist: NpmManifest,
        }
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
        self.meta.npm_manifest = serde_json::from_str::<W>(&resp)
            .expect("The package manifest file should be correct/valid JSON")
            .dist;
    }

    /// Returns the download directory for this package depending on wether it is indirect or not.
    fn download_dir(&self) -> String {
        if self.meta.indirect {
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
/// Converts a absolute path to a relative path, with a cwd.
/// `a/b/c`, cwd `b` => `./c`.
fn absolute_to_relative(path: &String, cwd: &String) -> String {
    let mut common = cwd.clone();
    let mut result = String::from("");
    while path.trim_start_matches(&common) == path {
        common = Path::new(&common)
            .parent()
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .to_string();
        result = if result.is_empty() {
            String::from("..")
        } else {
            format!("../{result}")
        };
    }
    let uncommon = path.trim_start_matches(&common);
    if !(result.is_empty() && uncommon.is_empty()) {
        result.push_str(uncommon);
    } else if !uncommon.is_empty() {
        result = uncommon[1..].into();
    }
    result
}

impl Package {
    /// Gets the dependencies of this [Package], placing them in `self.meta.dependencies`.
    fn get_deps(&mut self) -> &Vec<Package> {
        let cfg = self.get_config_file();
        cfg.dependencies.into_iter().for_each(|mut dep| {
            dep.meta.indirect = true;
            self.meta.dependencies.push(dep);
        });
        &self.meta.dependencies
    }

    /// Modifies the loads of a GDScript script.
    /// ```gdscript
    /// extends Node
    ///
    /// const Wow = preload("res://addons/my_awesome_addon/wow.gd")
    /// ```
    /// =>
    /// ```gdscript
    /// # --snip--
    /// const Wow = preload("../my_awesome_addon/wow.gd")
    /// ```
    /// (depending on the supplied cwd)
    fn modify_script_loads(&self, t: &String, cwd: &String) -> String {
        lazy_static::lazy_static! {
            static ref SCRIPT_LOAD_R: Regex = Regex::new("(pre)?load\\([\"']([^)]+)['\"]\\)").unwrap();
        }
        SCRIPT_LOAD_R
            .replace_all(&t, |c: &Captures| {
                format!(
                    "{}load('{}')",
                    if c.get(1).is_some() { "pre" } else { "" },
                    self.modify_load(
                        String::from(c.get(2).unwrap().as_str().trim_start_matches("res://")),
                        c.get(1).is_some(),
                        cwd
                    )
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
    /// [ext_resource path="../my_awesome_addon/wow.gd" type="Script" id=1]
    /// ```
    /// depending on supplied cwd.
    /// godot will automatically re-absolute-ify the path, but that is fine.
    fn modify_tres_loads(&self, t: &String, cwd: &String) -> String {
        lazy_static::lazy_static! {
            static ref TRES_LOAD_R: Regex = Regex::new("[ext_resource path=\"([^\"]+)\"").unwrap();
        }
        TRES_LOAD_R
            .replace_all(&t, |c: &Captures| {
                format!(
                    "[ext_resource path=\"{}\"",
                    self.modify_load(
                        String::from(c.get(1).unwrap().as_str().trim_start_matches("res://")),
                        false,
                        cwd
                    )
                )
            })
            .to_string()
    }

    /// The backend for modify_script_loads and modify_tres_loads.
    fn modify_load(&self, path: String, relative_allowed: bool, cwd: &String) -> String {
        let path_p = Path::new(&path);
        if path_p.exists() || Path::new(cwd).join(path_p).exists() {
            if relative_allowed {
                let rel = absolute_to_relative(&path, cwd);
                if path.len() > rel.len() {
                    return rel;
                }
            }
            return format!("res://{path}");
        }
        if let Some(c) = path_p.components().nth(1) {
            let mut cfg = HashMap::<String, String>::new();
            for pkg in &self.meta.dependencies {
                cfg.insert(pkg.name.clone(), pkg.download_dir());
                if let Some((_, s)) = pkg.name.split_once("/") {
                    cfg.insert(String::from(s), pkg.download_dir()); // unscoped (@ben/cli => cli) (for compat)
                }
            }
            cfg.insert(self.name.clone(), self.download_dir());
            if let Some((_, s)) = self.name.split_once("/") {
                cfg.insert(String::from(s), self.download_dir());
            }
            if let Some(path) = cfg.get(&String::from(c.as_os_str().to_str().unwrap())) {
                let p = format!("res://{path}");
                if relative_allowed {
                    let rel = absolute_to_relative(path, cwd);
                    if p.len() > rel.len() {
                        return rel;
                    }
                }
                return p;
            }
        };
        println!("Could not find path for {}", path);
        return format!("res://{path}");
    }

    /// Recursively modifies a directory.
    fn recursive_modify(&self, dir: String, deps: &Vec<Package>) -> io::Result<()> {
        for entry in read_dir(&dir)? {
            let p = entry?;
            if p.path().is_dir() {
                self.recursive_modify(
                    format!("{dir}/{}", p.file_name().into_string().unwrap()),
                    deps,
                )?;
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
                        Type::TextResource => self.modify_tres_loads(&text, &dir),
                        Type::GDScript => self.modify_script_loads(&text, &dir),
                    },
                )?;
            }
        }
        Ok(())
    }

    /// The catalyst for `recursive_modify`.
    pub fn modify(&self) {
        if self.is_installed() == false {
            panic!("Attempting to modify a package that is not installed");
        }
        if let Err(e) = self.recursive_modify(self.download_dir(), &self.meta.dependencies) {
            println!("Modification of {self} yielded error {e}");
        }
    }
}

impl fmt::Display for Package {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

impl fmt::Debug for Package {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.to_string())
    }
}
