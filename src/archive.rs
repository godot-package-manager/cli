use crate::config_file::*;
use crate::ctx;
use crate::package::Package;
use crate::Client;
use anyhow::{Context, Result};
use flate2::bufread::GzDecoder;
use serde::Serialize;
use std::fmt::Display;
use std::fs::{create_dir_all, set_permissions, File, Permissions};
use std::io::{self, prelude::*, Cursor};
use std::path::{Component::Normal, Path, PathBuf};
use tar::Archive as Tarchive;
use tar::EntryType::Directory;
use zip::result::{ZipError, ZipResult};
use zip::ZipArchive as Zarchive;

type TArch = Tarchive<GzDecoder<Cursor<Vec<u8>>>>;
type ZArch = Zarchive<Cursor<Vec<u8>>>;

#[derive(Default, Clone, Serialize, PartialEq, Eq, Ord, PartialOrd, Hash, Debug)]
pub struct Data {
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub uri: String,
}

impl Data {
    pub fn new(bytes: Vec<u8>, uri: String) -> Self {
        Self { bytes, uri }
    }
    pub fn new_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            uri: String::new(),
        }
    }
    pub fn new_uri(uri: String) -> Self {
        Self { bytes: vec![], uri }
    }
}

#[derive(Default, Clone, Serialize, PartialEq, Eq, Ord, PartialOrd, Hash, Debug)]
#[serde(untagged)]
pub enum CompressionType {
    Gzip(Data),
    Zip(Data),
    Lock(String),
    #[default]
    None,
}

impl Display for CompressionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionType::Gzip(d) => write!(f, "{}", d.uri),
            CompressionType::Zip(d) => write!(f, "{}", d.uri),
            CompressionType::Lock(d) => write!(f, "{}", d),
            _ => unreachable!(),
        }
    }
}

impl CompressionType {
    pub fn from(ty: &str, bytes: Vec<u8>, uri: String) -> Self {
        match ty {
            "zip" => Self::Zip(Data::new(bytes, uri)),
            _ => Self::Gzip(Data::new(bytes, uri)),
        }
    }

    pub fn lock(&mut self) {
        *self = Self::Lock(match self {
            CompressionType::Gzip(d) => std::mem::take(&mut d.uri),
            CompressionType::Zip(d) => std::mem::take(&mut d.uri),
            _ => unreachable!(),
        })
    }
}

enum ArchiveType {
    Gzip(Box<TArch>),
    Zip(ZArch),
}

pub struct Archive {
    inner: ArchiveType,
    uri: String,
}

// impl<'a, Z> From<TArch<'a>> for Archive<'a> {
//     fn from(value: TArch<'a>) -> Self {
//         Self::Gzip(value)
//     }
// }

// impl<'a> From<ZArch<'a>> for Archive<'a> {
//     fn from(value: ZArch<'a>) -> Self {
//         Self::Zip(value)
//     }
// }

fn unpack_zarchive(archive: &mut ZArch, dst: &Path) -> ZipResult<()> {
    if dst.symlink_metadata().is_err() {
        create_dir_all(dst).map_err(ZipError::Io)?;
    }
    let dst = &dst.canonicalize().unwrap_or(dst.to_path_buf());

    let mut directories = vec![];
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let path = dst.join(skip_toplevel(
            file.enclosed_name().ok_or(ZipError::FileNotFound)?,
        ));
        if file.is_dir() {
            directories.push(path);
        } else {
            create_dir_all(path.parent().unwrap())?;
            let mut outfile = File::create(&path)?;
            io::copy(&mut file, &mut outfile)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(mode) = file.unix_mode() {
                    set_permissions(&path, Permissions::from_mode(mode))?;
                }
            }
        }
    }
    for path in directories {
        create_dir_all(path)?;
    }
    Ok(())
}

fn skip_toplevel(p: &Path) -> PathBuf {
    p.components()
        .skip(1)
        .filter(|c| matches!(c, Normal(_)))
        .collect::<PathBuf>()
}

fn unpack_tarchive(archive: &mut TArch, dst: &Path) -> io::Result<()> {
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
        let mut entry = (dst.join(skip_toplevel(&entry.path()?)), entry);
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

fn get_zfile(zarchive: &mut ZArch, search: &str, out: &mut String) -> ZipResult<()> {
    for i in 0..zarchive.len() {
        let mut file = zarchive.by_index(i)?;
        if let Some(n) = file.enclosed_name() {
            if let Some(base) = &Path::new(n).file_name() {
                if base.to_string_lossy() == search {
                    file.read_to_string(out)?;
                    return Ok(());
                }
            }
        }
    }
    Err(ZipError::FileNotFound)
}

fn get_gfile(tarchive: &mut TArch, file: &str, out: &mut String) -> io::Result<()> {
    for entry in tarchive.entries()? {
        let mut entry = entry?;
        if let Ok(p) = entry.path() {
            if p.file_name().ok_or(io::ErrorKind::InvalidData)? == file {
                entry.read_to_string(out)?;
                return Ok(());
            }
        }
    }
    Err(io::ErrorKind::InvalidData.into())
}

impl Archive {
    pub fn unpack(&mut self, dst: &Path) -> Result<()> {
        match &mut self.inner {
            ArchiveType::Gzip(g) => unpack_tarchive(g, dst)?,
            ArchiveType::Zip(z) => unpack_zarchive(z, dst)?,
        }
        Ok(())
    }

    pub fn get_file(&mut self, file: &str, out: &mut String) -> Result<()> {
        match &mut self.inner {
            ArchiveType::Gzip(g) => get_gfile(g, file, out)?,
            ArchiveType::Zip(z) => get_zfile(z, file, out)?,
        }
        Ok(())
    }

    fn wrap(wrap: ArchiveType, uri: String) -> Self {
        Self { inner: wrap, uri }
    }

    pub fn new(value: CompressionType) -> Result<Self> {
        match value {
            CompressionType::Gzip(data) => Ok(Self::new_gzip(data.bytes, data.uri)),
            CompressionType::Zip(data) => Self::new_zip(data.bytes, data.uri),
            _ => unreachable!(),
        }
    }

    pub fn new_gzip(value: Vec<u8>, uri: String) -> Self {
        Self::wrap(
            ArchiveType::Gzip(Box::new(Tarchive::new(GzDecoder::new(Cursor::new(value))))),
            uri,
        )
    }

    pub fn new_zip(value: Vec<u8>, uri: String) -> Result<Self> {
        Ok(Self::wrap(
            ArchiveType::Zip(Zarchive::new(Cursor::new(value))?),
            uri,
        ))
    }
    /// async trait + lifetimes = boom
    pub async fn into_package(mut self, client: Client) -> Result<Package> {
        let mut contents = String::new();
        {
            ctx!(
                self.get_file("package.json", &mut contents),
                "searching for package.json"
            )?;
        }
        let ty = match self.inner {
            ArchiveType::Zip(_) => CompressionType::Zip(Data::new_uri(self.uri)),
            ArchiveType::Gzip(_) => CompressionType::Gzip(Data::new_uri(self.uri)),
        };
        ctx!(
            ctx!(
                ConfigFile::parse(&contents, ConfigType::JSON, client).await,
                "parsing config file from package.json inside zipfile"
            )?
            .into_package(ty),
            "turning config file into package"
        )
    }
}
