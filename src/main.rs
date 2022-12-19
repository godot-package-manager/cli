mod config_file;
mod npm;
mod package;

use crate::package::Package;
use clap::Parser;
use config_file::ConfigFile;
use std::env::current_dir;
use std::fs::{create_dir, read_dir, remove_dir};
use std::io::Result;
use std::panic;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "gpm")]
#[command(about = "A package manager for godot.", long_about = None)]
#[command(bin_name = "gpm")]
/// A package mnanager for godot
struct Args {
    #[command(subcommand)]
    action: Actions,
    #[arg(
        short = 'c',
        long = "cfg-file",
        default_value = "godot.package",
        global = true
    )]
    config_file: PathBuf,
}

#[derive(clap::Subcommand)]
enum Actions {
    #[command(about = "Update all wanted packaes. Installs packages if they don't yet exist.")]
    #[clap(short_flag = 'u')]
    Update,
    #[command(about = "Deletes all installed packages.")]
    #[clap(short_flag = 'p')]
    Purge,
    #[command(
        about = "Prints a tree of all the wanted packages, and their dependencies.",
        long_about = "
Print a tree of all the wanted packages, and their dependencies.
Produces output like
/home/my-package
└── @bendn/test@2.0.10
    └── @bendn/gdcli@1.2.5"
    )]
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
    let cfg_file = ConfigFile::new(args.config_file);
    match args.action {
        Actions::Update => update(cfg_file),
        Actions::Purge => purge(cfg_file),
        Actions::Tree => tree(cfg_file),
    }
    println!("Finished");
}

fn update(mut cfg: ConfigFile) {
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
    cfg.for_each(|p| p.download());
    cfg.lock();
}

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

fn purge(mut cfg: ConfigFile) {
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
    cfg.lock();
}

fn tree(cfg: ConfigFile) {
    if let Ok(s) = current_dir() {
        println!("{}", s.to_string_lossy().to_string());
    } else {
        println!(".");
    };
    iter(cfg.packages, "");
    fn iter(packages: Vec<Package>, prefix: &str) {
        let mut index = packages.len();
        for p in packages {
            let name = p.to_string();
            index -= 1;
            println!("{prefix}{} {name}", if index != 0 { "├──" } else { "└──" });
            if p.has_deps() {
                iter(
                    p.meta.dependencies,
                    &format!("{prefix}{}   ", if index != 0 { '│' } else { ' ' }),
                );
            }
        }
    }
}
