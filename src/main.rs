mod config_file;
mod package;

use crate::package::Package;
use clap::Parser;
use config_file::ConfigFile;
use std::env::current_dir;
use std::fs::{create_dir, read_dir, read_to_string, remove_dir, write};
use std::io::{stdin, Read, Result};
use std::panic;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "gpm")]
#[command(bin_name = "gpm")]
/// A package manager for godot.
struct Args {
    #[command(subcommand)]
    action: Actions,
    #[arg(
        short = 'c',
        long = "cfg-file",
        default_value = "godot.package",
        global = true
    )]
    /// Specify the location of the package configuration file (https://github.com/godot-package-manager#godotpackage). If -, read from stdin.
    config_file: PathBuf,
    #[arg(
        short = 'l',
        long = "lock-file",
        default_value = "godot.lock",
        global = true
    )]
    /// Specify the location of the lock file. If -, print to stdout.
    lock_file: PathBuf,
}

#[derive(clap::Subcommand)]
enum Actions {
    #[clap(short_flag = 'u')]
    /// Downloads the latest versions of your wanted packages.
    Update,
    #[clap(short_flag = 'p')]
    /// Deletes all installed packages.
    Purge,
    /// Prints a tree of all the wanted packages, and their dependencies.
    #[command(long_about = "
Print a tree of all the wanted packages, and their dependencies.
Produces output like
/home/my-package
└── @bendn/test@2.0.10
    └── @bendn/gdcli@1.2.5")]
    Tree,
}

fn main() {
    #[rustfmt::skip]
    panic::set_hook(Box::new(|panic_info| {
        const RED: &str = "\x1b[1;31m";
        const RESET: &str = "\x1b[0m";
        match panic_info.location() {
            Some(s) => print!("{RED}err{RESET}@{}:{}:{}: ", s.file(), s.line(), s.column()),
            None => print!("{RED}err{RESET}: "),
        }
        if let Some(s) = panic_info.payload().downcast_ref::<&str>() { println!("{s}"); }
        else if let Some(s) = panic_info.payload().downcast_ref::<String>() { println!("{s}"); }
        else { println!("unknown"); };
    }));
    let args = Args::parse();
    let mut contents = String::from("");
    if args.config_file == Path::new("-") {
        let bytes = stdin()
            .read_to_string(&mut contents)
            .expect("Stdin read should be ok");
        if bytes == 0 {
            panic!("Stdin should not be empty");
        };
    } else {
        contents = read_to_string(args.config_file).expect("Reading config file should be ok");
    };
    let mut cfg_file = ConfigFile::new(&contents);
    match args.action {
        Actions::Update => update(&mut cfg_file, true),
        Actions::Purge => purge(&mut cfg_file),
        Actions::Tree => print!("{}", tree(&cfg_file)),
    }
    let lockfile = cfg_file.lock();
    if args.lock_file == Path::new("-") {
        println!("{lockfile}");
    } else {
        write(args.lock_file, lockfile).expect("Writing lock file should be ok");
    }
}

fn update(cfg: &mut ConfigFile, modify: bool) {
    if !Path::new("./addons/").exists() {
        create_dir("./addons/").expect("Should be able to create addons folder");
    }
    if cfg.packages.is_empty() {
        panic!("No packages to update (modify the \"godot.package\" file to add packages)");
    }
    println!(
        "Update {} package{}",
        cfg.packages.len(),
        if cfg.packages.len() > 1 { "s" } else { "" }
    );
    cfg.for_each(|p| {
        p.download();
        if modify {
            p.modify()
        }
    });
}

/// Recursively deletes empty directories.
/// With this fs tree:
/// ```
/// .
/// `-- dir0
///      |-- dir1
///      `-- dir2
/// ```
/// dir 1 and 2 will be deleted.
/// Run multiple times to delete `dir0`.
fn recursive_delete_empty(dir: String) -> Result<()> {
    if read_dir(&dir)?.next().is_none() {
        return remove_dir(dir);
    }
    for p in read_dir(&dir)?.filter_map(|e| {
        let e = e.ok()?;
        e.file_type().ok()?.is_dir().then_some(e)
    }) {
        recursive_delete_empty(format!("{dir}/{}", p.file_name().to_string_lossy()))?;
    }
    Ok(())
}

fn purge(cfg: &mut ConfigFile) {
    let packages = cfg
        .collect()
        .into_iter()
        .filter(|p| p.is_installed())
        .collect::<Vec<Package>>();
    if packages.is_empty() {
        if cfg.packages.is_empty() {
            panic!("No packages to update (modify the \"godot.package\" file to add packages)")
        } else {
            panic!("No packages installed(use \"gpm --update\" to install packages)")
        };
    };
    println!(
        "Purge {} package{}",
        packages.len(),
        if packages.len() > 1 { "s" } else { "" }
    );
    packages.into_iter().for_each(|p| p.purge());

    // run multiple times because the algorithm goes from top to bottom, stupidly.
    for _ in 0..3 {
        if let Err(e) = recursive_delete_empty("./addons".to_string()) {
            eprintln!("Unable to remove empty directorys: {e}")
        }
    }
}

fn tree(cfg: &ConfigFile) -> String {
    let mut tree: String = if let Ok(s) = current_dir() {
        format!("{}\n", s.to_string_lossy())
    } else {
        ".\n".to_string()
    };
    iter(&cfg.packages, "", &mut tree);
    fn iter(packages: &Vec<Package>, prefix: &str, tree: &mut String) {
        // the index is used to decide if the package is the last package,
        // so we can use a corner instead of a T.
        let mut index = packages.len();
        for p in packages {
            let name = p.to_string();
            index -= 1;
            tree.push_str(
                format!(
                    "{prefix}{} {name}\n",
                    if index != 0 { "├──" } else { "└──" }
                )
                .as_str(),
            );
            if p.has_deps() {
                iter(
                    &p.dependencies,
                    &format!("{prefix}{}   ", if index != 0 { '│' } else { ' ' }),
                    tree,
                );
            }
        }
    }
    tree
}

#[cfg(test)]
mod test_utils {
    use glob::glob;
    use sha2::{Digest, Sha256};
    use std::{env::set_current_dir, fs::create_dir, fs::read};
    use tempdir::TempDir;

    pub fn mktemp() -> TempDir {
        let tmp_dir = TempDir::new("gpm-tests").unwrap();
        set_current_dir(tmp_dir.path()).unwrap();
        create_dir("addons").unwrap();
        tmp_dir
    }

    pub fn hashd(d: &str) -> Vec<String> {
        let mut files = glob(format!("{}/**/*", d).as_str())
            .unwrap()
            .into_iter()
            .filter_map(|s| {
                let p = &s.unwrap();
                p.is_file().then(|| {
                    let mut hasher = Sha256::new();
                    hasher.update(read(p).unwrap());
                    format!("{:x}", &hasher.finalize())
                })
            })
            .collect::<Vec<String>>();
        files.sort();
        files
    }
}

#[test]
fn gpm() {
    let _t = test_utils::mktemp();
    let cfg_file = &mut config_file::ConfigFile::new(&r#"packages: {"@bendn/test":2.0.10}"#.into());
    update(cfg_file, false);
    assert_eq!(test_utils::hashd("addons").join("|"), "1c2fd93634817a9e5f3f22427bb6b487520d48cf3cbf33e93614b055bcbd1329|8e77e3adf577d32c8bc98981f05d40b2eb303271da08bfa7e205d3f27e188bd7|a625595a71b159e33b3d1ee6c13bea9fc4372be426dd067186fe2e614ce76e3c|c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e|c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e|c850a9300388d6da1566c12a389927c3353bf931c4d6ea59b02beb302aac03ea|d060936e5f1e8b1f705066ade6d8c6de90435a91c51f122905a322251a181a5c|d711b57105906669572a0e53b8b726619e3a21463638aeda54e586a320ed0fc5|d794f3cee783779f50f37a53e1d46d9ebbc5ee7b37c36d7b6ee717773b6955cd|e4f9df20b366a114759282209ff14560401e316b0059c1746c979f478e363e87");
    purge(cfg_file);
    assert_eq!(test_utils::hashd("addons"), vec![] as Vec<String>);
    assert_eq!(
        tree(cfg_file)
            .lines()
            .skip(1)
            .collect::<Vec<&str>>()
            .join("\n"),
        "└── @bendn/test@2.0.10\n    └── @bendn/gdcli@1.2.5"
    );
}
