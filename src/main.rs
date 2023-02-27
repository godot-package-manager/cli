mod config_file;
mod package;
mod theme;

use crate::package::Package;
use anyhow::Result;
use clap::{ColorChoice, Parser, Subcommand, ValueEnum};
use config_file::{ConfigFile, ConfigType};
use console::{self, Term};
use indicatif::{HumanCount, HumanDuration, ProgressBar, ProgressIterator};
use std::fs::{create_dir, read_dir, read_to_string, remove_dir, write};
use std::io::{stdin, Read};
use std::path::{Path, PathBuf};
use std::{env::current_dir, panic, time::Instant};
use rayon::prelude::*;

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

    #[arg(long = "colors", default_value = "auto", global = true)]
    /// Control color output.
    colors: ColorChoice,

    #[arg(long = "nv", global = true)]
    /// Disable progress bars
    not_verbose: bool,
}

#[derive(Subcommand)]
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
    Tree {
        #[arg(value_enum, default_value = "utf8", long = "charset")]
        /// Character set to print in.
        charset: CharSet,

        #[arg(value_enum, default_value = "indent", long = "prefix")]
        /// The prefix (indentation) of how the tree entrys are displayed.
        prefix: PrefixType,

        #[arg(long = "tarballs", default_value = "false")]
        /// To print download urls next to the package name.
        print_tarballs: bool,
    },
    Init {
        #[arg(long = "packages", num_args = 0..)]
        packages: Vec<Package>,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
/// Charset for the tree subcommand.
enum CharSet {
    /// Unicode characters (├── └──).
    UTF8,
    /// ASCII characters (|-- `--).
    ASCII,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
/// Prefix type for the tree subcommand.
enum PrefixType {
    /// Indents the tree entries proportional to the depth.
    Indent,
    /// Print the depth before the entries.
    Depth,
    /// No indentation, just list.
    None,
}

fn main() {
    panic::set_hook(Box::new(|panic_info| {
        eprint!("{:>12} ", putils::err());
        if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            eprint!("{s}");
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            eprint!("{s}");
        } else {
            eprint!("unknown");
        };
        if let Some(s) = panic_info.location() {
            eprint!(" (@{}:{})", s.file(), s.line())
        }
        eprintln!("");
    }));
    let args = Args::parse();
    fn set_colors(val: bool) {
        console::set_colors_enabled(val);
        console::set_colors_enabled_stderr(val)
    }
    match args.colors {
        ColorChoice::Always => set_colors(true),
        ColorChoice::Never => set_colors(false),
        ColorChoice::Auto => set_colors(Term::stdout().is_term() && Term::stderr().is_term()),
    }
    fn get_cfg(path: PathBuf) -> ConfigFile {
        let mut contents = String::from("");
        if path == Path::new("-") {
            let bytes = stdin()
                .read_to_string(&mut contents)
                .expect("Stdin read should be ok");
            if bytes == 0 {
                panic!("Stdin should not be empty");
            };
        } else {
            contents = read_to_string(path).expect("Reading config file should be ok");
        };
        ConfigFile::new(&contents)
    }
    fn lock(cfg: &mut ConfigFile, path: PathBuf) {
        let lockfile = cfg.lock();
        if path == Path::new("-") {
            println!("{lockfile}");
        } else {
            write(path, lockfile).expect("Writing lock file should be ok");
        }
    }
    match args.action {
        Actions::Update => {
            let c = &mut get_cfg(args.config_file);
            update(c, true, args.not_verbose);
            lock(c, args.lock_file);
        }
        Actions::Purge => {
            let c = &mut get_cfg(args.config_file);
            purge(c, args.not_verbose);
            lock(c, args.lock_file);
        }
        Actions::Tree {
            charset,
            prefix,
            print_tarballs,
        } => print!(
            "{}",
            tree(
                &mut get_cfg(args.config_file), // no locking needed
                charset,
                prefix,
                print_tarballs
            )
        ),
        Actions::Init { packages } => init(packages).expect("Initializing cfg should be ok"),
    }
}

fn update(cfg: &mut ConfigFile, modify: bool, not_verbose: bool) {
    if !Path::new("./addons/").exists() {
        create_dir("./addons/").expect("Should be able to create addons folder");
    }
    let packages = cfg.collect();
    if packages.is_empty() {
        panic!("No packages to update (modify the \"godot.package\" file to add packages)");
    }
    let bar;
    let p_count = packages.len() as u64;
    if not_verbose {
        bar = ProgressBar::hidden();
    } else {
        bar = putils::bar(p_count);
        bar.set_prefix("Updating");
    };
    let now = Instant::now();
    packages
        .into_par_iter()
        .for_each(|mut p| {
            bar.set_message(format!("{p}"));
            p.download();
            if modify {
                if let Err(e) = p.modify() {
                    bar.suspend(|| {
                        eprintln!(
                            "{:>12} modification of {p} failed with err {e}",
                            putils::warn()
                        )
                    });
                }
            }
            bar.inc(1);
            bar.suspend(|| println!("{:>12} {p}", putils::green("Downloaded")));
        });
    println!(
        "{:>12} updated {} package{} in {}",
        putils::green("Finished"),
        HumanCount(p_count),
        if p_count > 0 { "s" } else { "" },
        HumanDuration(now.elapsed())
    )
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
fn recursive_delete_empty(dir: String) -> std::io::Result<()> {
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

fn purge(cfg: &mut ConfigFile, not_verbose: bool) {
    let packages = cfg
        .collect()
        .into_iter()
        .filter(|p| p.is_installed())
        .collect::<Vec<Package>>();
    if packages.is_empty() {
        if cfg.packages.is_empty() {
            panic!("No packages configured (modify the \"godot.package\" file to add packages)")
        } else {
            panic!("No packages installed (use \"gpm --update\" to install packages)")
        };
    };
    let p_count = packages.len() as u64;
    let bar;
    if not_verbose {
        bar = ProgressBar::hidden();
    } else {
        bar = putils::bar(p_count);
        bar.set_prefix("Purging");
    }
    let now = Instant::now();
    packages
        .into_iter()
        .progress_with(bar.clone()) // the last steps
        .for_each(|p| {
            bar.set_message(format!("{p}"));
            bar.println(format!(
                "{:>12} {p} ({})",
                putils::green("Deleting"),
                p.download_dir(),
            ));
            p.purge()
        });

    // run multiple times because the algorithm goes from top to bottom, stupidly.
    for _ in 0..3 {
        if let Err(e) = recursive_delete_empty("./addons".to_string()) {
            eprintln!("Unable to remove empty directorys: {e}")
        }
    }
    println!(
        "{:>12} purge {} package{} in {}",
        putils::green("Finished"),
        HumanCount(p_count),
        if p_count > 0 { "s" } else { "" },
        HumanDuration(now.elapsed())
    )
}

fn tree(
    cfg: &mut ConfigFile,
    charset: CharSet,
    prefix: PrefixType,
    print_tarballs: bool,
) -> String {
    let mut tree: String = if let Ok(s) = current_dir() {
        format!("{}\n", s.to_string_lossy())
    } else {
        ".\n".to_string()
    };
    let mut count: u64 = 0;
    iter(
        &mut cfg.packages,
        "",
        &mut tree,
        match charset {
            CharSet::UTF8 => "├──", // believe it or not, these are quite unlike
            CharSet::ASCII => "|--",      // its hard to tell, with ligatures enable
        },
        match charset {
            CharSet::UTF8 => "└──",
            CharSet::ASCII => "`--",
        },
        prefix,
        print_tarballs,
        0,
        &mut count,
    );
    tree.push_str(format!("{} dependencies", HumanCount(count)).as_str());

    fn iter(
        packages: &mut Vec<Package>,
        prefix: &str,
        tree: &mut String,
        t: &str,
        l: &str,
        prefix_type: PrefixType,
        print_tarballs: bool,
        depth: u32,
        count: &mut u64,
    ) {
        // the index is used to decide if the package is the last package,
        // so we can use a L instead of a T.
        let mut tmp: String;
        let mut index = packages.len();
        *count += index as u64;
        for p in packages {
            let name = p.to_string();
            index -= 1;
            tree.push_str(
                match prefix_type {
                    PrefixType::Indent => {
                        format!("{prefix}{} {name}", if index != 0 { t } else { l })
                    }
                    PrefixType::Depth => format!("{depth} {name}"),
                    PrefixType::None => format!("{name}"),
                }
                .as_str(),
            );
            if print_tarballs {
                tree.push(' ');
                tree.push_str(
                    p.get_tarball()
                        .expect("Should be able to get tarball")
                        .as_str(),
                );
            }
            tree.push('\n');
            if p.has_deps() {
                iter(
                    &mut p.dependencies,
                    if prefix_type == PrefixType::Indent {
                        tmp = format!("{prefix}{}   ", if index != 0 { '│' } else { ' ' });
                        tmp.as_str()
                    } else {
                        ""
                    },
                    tree,
                    t,
                    l,
                    prefix_type,
                    print_tarballs,
                    depth + 1,
                    count,
                );
            }
        }
    }
    tree
}

fn init(mut packages: Vec<Package>) -> Result<()> {
    let mut c = ConfigFile::default();
    if packages.is_empty() && putils::confirm("Add a package?", true)? {
        while {
            packages.push(putils::input("Package?")?);
            putils::confirm("Add another package?", true)?
        } {}
    };
    c.packages = packages;
    let types = vec![ConfigType::JSON, ConfigType::YAML, ConfigType::TOML];

    let mut path = Path::new(&putils::input_with_default::<String>(
        "Config file save location?",
        "godot.package".into(),
    )?)
    .to_path_buf();
    while path.exists() {
        if putils::confirm("This file already exists. Replace?", false)? {
            break;
        } else {
            path = Path::new(&putils::input::<String>("Config file save location?")?).to_path_buf();
        }
    }
    while write(&path, "").is_err() {
        path = Path::new(&putils::input_with_default::<String>(
            "Chosen file not accessible, try again:",
            "godot.package".into(),
        )?)
        .to_path_buf();
    }
    let c_text = c
        .clone()
        .print(types[putils::select(&types, "Language to save in:", 2)?]);
    write(path, c_text)?;
    if putils::confirm("Would you like to view the dependency tree?", true)? {
        println!("{}", tree(&mut c, CharSet::UTF8, PrefixType::Indent, false));
    };

    if c.packages.len() > 0
        && putils::confirm("Would you like to install your new packages?", true)?
    {
        update(&mut c, true, false);
    };
    println!("Goodbye!");
    Ok(())
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
    update(cfg_file, false, false);
    assert_eq!(test_utils::hashd("addons").join("|"), "1c2fd93634817a9e5f3f22427bb6b487520d48cf3cbf33e93614b055bcbd1329|8e77e3adf577d32c8bc98981f05d40b2eb303271da08bfa7e205d3f27e188bd7|a625595a71b159e33b3d1ee6c13bea9fc4372be426dd067186fe2e614ce76e3c|c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e|c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e|c850a9300388d6da1566c12a389927c3353bf931c4d6ea59b02beb302aac03ea|d060936e5f1e8b1f705066ade6d8c6de90435a91c51f122905a322251a181a5c|d711b57105906669572a0e53b8b726619e3a21463638aeda54e586a320ed0fc5|d794f3cee783779f50f37a53e1d46d9ebbc5ee7b37c36d7b6ee717773b6955cd|e4f9df20b366a114759282209ff14560401e316b0059c1746c979f478e363e87");
    purge(cfg_file, false);
    assert_eq!(test_utils::hashd("addons"), vec![] as Vec<String>);
    assert_eq!(
        tree(
            cfg_file,
            crate::CharSet::UTF8,
            crate::PrefixType::Indent,
            false
        )
        .lines()
        .skip(1)
        .collect::<Vec<&str>>()
        .join("\n"),
        "└── @bendn/test@2.0.10\n    └── @bendn/gdcli@1.2.5\n2 dependencies"
    );
}

/// Print utilities.
/// Remember to use {:>12}
pub mod putils {
    use crate::theme::BasicTheme;
    use console::{style, StyledObject};
    use dialoguer::{Confirm, Input, Select};
    use indicatif::{ProgressBar, ProgressStyle};
    use std::io::Result;
    use std::str::FromStr;

    #[inline]
    pub fn err() -> StyledObject<String> {
        style(format!("Error")).red().bold()
    }

    #[inline]
    pub fn select<T: ToString>(items: &Vec<T>, p: &str, default: usize) -> Result<usize> {
        Select::with_theme(&BasicTheme::default())
            .items(items)
            .with_prompt(p)
            .default(default)
            .interact()
    }

    #[inline]
    pub fn confirm(p: &str, default: bool) -> Result<bool> {
        // TODO: theme
        Ok(Confirm::with_theme(&BasicTheme::default())
            .with_prompt(p)
            .default(default)
            .interact()?)
    }

    #[inline]
    pub fn input<T>(p: &str) -> Result<T>
    where
        T: Clone + ToString + FromStr,
        <T as FromStr>::Err: std::fmt::Debug + ToString,
    {
        Input::with_theme(&BasicTheme::default())
            .with_prompt(p)
            .interact_text()
    }

    #[inline]
    pub fn input_with_default<T>(p: &str, d: T) -> Result<T>
    where
        T: Clone + ToString + FromStr,
        <T as FromStr>::Err: std::fmt::Debug + ToString,
    {
        Input::with_theme(&BasicTheme::default())
            .with_prompt(p)
            .default(d)
            .interact_text()
    }

    #[inline]
    pub fn warn() -> StyledObject<String> {
        style(format!("Warn")).yellow().bold()
    }

    #[inline]
    pub fn green(t: &str) -> StyledObject<&str> {
        style(t).green().bold()
    }

    #[inline]
    pub fn bar(len: u64) -> ProgressBar {
        let bar = ProgressBar::new(len);
        bar.set_style(
            ProgressStyle::with_template(
                "{prefix:>12.cyan.bold} [{bar:20.green}] {human_pos}/{human_len}: {wide_msg}",
            )
            .unwrap()
            .progress_chars("-> "),
        );
        bar
    }
}
