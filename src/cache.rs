use crate::package::parsing::{Packument, ParsedManifest, ParsedPackage};
use crate::package::Package;
use crate::{ctx, Client};

use anyhow::{Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::mapref::multiple::{RefMulti, RefMutMulti};
use dashmap::mapref::one::{Ref, RefMut};
use dashmap::DashMap;
use semver_rs::{Range, Version};
use std::sync::Arc;

type O<'a, T> = Option<Ref<'a, String, T>>;
pub type R<'a> = RefMutMulti<'a, String, CacheEntry>;

#[derive(Clone)]
pub struct Cache {
    inner: Arc<DashMap<String, VersionsCache>>,
}

#[derive(Default, Clone)]
pub struct VersionsCache {
    inner: DashMap<String, CacheEntry>,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::default()),
        }
    }

    /// Deadlocks when mutable reference held
    pub fn get(&self, name: &str) -> O<VersionsCache> {
        self.inner.get(name)
    }
    /// Deadlocks when reference held
    pub fn get_mut(&self, name: &str) -> Option<RefMut<String, VersionsCache>> {
        self.inner.get_mut(name)
    }
    /// Deadlocks when reference held
    pub fn insert(&self, name: String, version: String, entry: CacheEntry) {
        self.inner.entry(name).or_default().insert(version, entry);
    }
    /// Deadlocks when reference held
    pub fn entry(&self, name: String) -> Entry<'_, String, VersionsCache> {
        self.inner.entry(name)
    }
}

impl VersionsCache {
    pub fn insert_packument(&mut self, pack: Packument) -> &mut Self {
        for manif in pack.versions {
            self.insert(manif.version.clone(), manif.into())
        }
        self
    }
    pub fn iter_versions(
        &mut self,
    ) -> impl Iterator<Item = (Version, RefMutMulti<String, CacheEntry>)> {
        self.iter_mut()
            .map(|x| (Self::version_of(x.key(), x.value().clone()), x))
    }

    pub fn get(&self, v: &str) -> Option<Ref<String, CacheEntry>> {
        self.inner.get(v)
    }

    pub fn versions(&self) -> impl Iterator<Item = Version> + '_ {
        self.iter()
            .map(|x| Self::version_of(x.key(), x.value().clone()))
    }

    fn version_of(k: &str, entry: CacheEntry) -> Version {
        if let CacheEntry::Parsed(p) = entry {
            p.manifest.version
        } else {
            Version::new(k).parse().unwrap()
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = RefMulti<String, CacheEntry>> {
        self.inner.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = R> {
        self.inner.iter_mut()
    }

    pub fn insert(&mut self, k: String, v: CacheEntry) {
        self.inner.insert(k, v);
    }

    #[must_use]
    /// if found and unparsed, swaps unparsed for parsed
    pub fn find_version(&mut self, v: &Range) -> Option<R> {
        let mut newest = None;
        for (version, entry) in self.iter_versions() {
            if v.test(&version) {
                // if v.exact() { return immediately }
                if let Some((_, v)) = &newest {
                    if version.cmp(v) == std::cmp::Ordering::Less {
                        continue;
                    }
                }
                newest = Some((entry, version))
            }
        }
        // todo: reuse this parsed version
        if let Some((e, _)) = newest {
            return Some(e);
        }
        None
    }
}

impl std::fmt::Debug for VersionsCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut iter = self.versions();
        if let Some(first) = iter.next() {
            write!(f, "{first}")?;
            for elem in iter {
                write!(f, ", {elem}")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)] // yuck, a clone
pub enum CacheEntry {
    Unparsed(ParsedPackage),
    Parsed(Package),
    Manifest(ParsedManifest),
    #[default]
    Empty,
}

impl From<Package> for CacheEntry {
    fn from(value: Package) -> Self {
        Self::Parsed(value)
    }
}
impl From<ParsedManifest> for CacheEntry {
    fn from(value: ParsedManifest) -> Self {
        Self::Manifest(value)
    }
}
impl From<ParsedPackage> for CacheEntry {
    fn from(value: ParsedPackage) -> Self {
        Self::Unparsed(value)
    }
}

impl CacheEntry {
    pub async fn parse(&mut self, client: Client, cache: Cache, name: String) -> Result<()> {
        match self {
            CacheEntry::Unparsed(p) => {
                let p = std::mem::take(p).into_package(client, cache).await?;
                *self = CacheEntry::Parsed(p);
            }
            CacheEntry::Manifest(m) => {
                let m = ctx!(
                    std::mem::take(m).into_manifest(client, cache).await,
                    "parsing ParsedManifest into Manifest in get_package()"
                )?;
                let p = Package::from_manifest(m, name.clone());
                *self = CacheEntry::Parsed(p);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn get_package(&self) -> Package {
        match self {
            CacheEntry::Parsed(p) => p.clone(),
            _ => unreachable!(),
        }
    }
}
