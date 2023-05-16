mod config_file;
mod package;
mod theme;
mod verbosity;

use config_file::{create_cache, Cache, ConfigFile, ConfigType};
use package::parsing::{IntoPackageList, ParsedPackage};
use package::Package;

use anyhow::Result;
use async_recursion::async_recursion;
use clap::{ColorChoice, Parser, Subcommand, ValueEnum};
use console::{self, Term};
use futures::stream::{self, StreamExt};
use indicatif::{HumanCount, HumanDuration, ProgressBar, ProgressIterator};
use lazy_static::lazy_static;
use reqwest::Client;
use std::collections::HashSet;
use std::fs::{create_dir, read_dir, read_to_string, remove_dir, write};
use std::io::{stdin, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::thread;
use std::{env::current_dir, panic, time::Instant};
use verbosity::Verbosity;

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
    #[arg(
        long = "verbosity",
        short = 'v',
        global = true,
        default_value = "normal"
    )]
    /// Verbosity level
    verbosity: Verbosity,
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
    /// Helpful initializer for the godot.package file.
    Init {
        #[arg(long = "packages", num_args = 0..)]
        packages: Vec<ParsedPackage>,
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

/// number of buffer slots
const PARALLEL: usize = 6;
lazy_static! {
    static ref BEGIN: Instant = Instant::now();
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
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
        eprintln!();
    }));
    let args = Args::parse();
    fn set_colors(val: bool) {
        console::set_colors_enabled(val);
        console::set_colors_enabled_stderr(val);
    }
    match args.colors {
        ColorChoice::Always => set_colors(true),
        ColorChoice::Never => set_colors(false),
        ColorChoice::Auto => set_colors(Term::stdout().is_term() && Term::stderr().is_term()),
    }
    let cache = create_cache();
    let client = mkclient();
    let mut cfg = {
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
        ConfigFile::new(&contents, client.clone(), cache.clone()).await
    };
    fn lock(cfg: &mut ConfigFile, path: PathBuf, cwd: &Path) {
        let lockfile = cfg.lock(cwd);
        if path == Path::new("-") {
            println!("{lockfile}");
        } else {
            write(path, lockfile).expect("Writing lock file should be ok");
        }
    }
    let _ = BEGIN.elapsed(); // needed to initialize the instant for whatever reason
    let cwd = current_dir().expect("Should be able to read cwd");
    match args.action {
        Actions::Update => {
            update(&mut cfg, true, args.verbosity, client.clone(), &cwd).await;
            lock(&mut cfg, args.lock_file, &cwd);
        }
        Actions::Purge => {
            purge(&mut cfg, args.verbosity, &cwd);
            lock(&mut cfg, args.lock_file, &cwd);
        }
        Actions::Tree {
            charset,
            prefix,
            print_tarballs,
        } => println!(
            "{}",
            tree(
                &mut cfg, // no locking needed
                charset,
                prefix,
                print_tarballs,
                client
            )
            .await
        ),
        Actions::Init { packages } => {
            init(
                packages
                    .into_package_list(client.clone(), cache.clone())
                    .await
                    .expect("Failed to parse `init` packages"),
                client,
                cache,
                &cwd,
            )
            .await
            .expect("Initializing cfg should be ok");
        }
    }
}

pub fn mkclient() -> Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "User-Agent",
        format!(
            "gpm/{} (godot-package-manager/cli on GitHub)",
            env!("CARGO_PKG_VERSION")
        )
        .parse()
        .unwrap(),
    );
    Client::builder().default_headers(headers).build().unwrap()
}

async fn update(cfg: &mut ConfigFile, modify: bool, v: Verbosity, client: Client, cwd: &Path) {
    if !cwd.join("addons").exists() {
        create_dir(cwd.join("addons")).expect("Should be able to create addons folder");
    }
    let packages = cfg.collect();
    if v.debug() {
        println!(
            "collecting {} packages took {}",
            packages.len(),
            HumanDuration(BEGIN.elapsed())
        );
        print!("packages: [");
        let mut first = true;
        for p in &packages {
            if first {
                print!("{p}");
            } else {
                print!(", {p}");
            }
            first = false;
        }
        println!("]");
    }

    if packages.is_empty() {
        panic!("No packages to update (modify the \"godot.package\" file to add packages)");
    }
    let bar;
    let p_count = packages.len() as u64;
    if v.bar() {
        bar = putils::bar(p_count);
        bar.set_prefix("Updating");
    } else {
        bar = ProgressBar::hidden();
    };
    enum Status {
        Processing(String),
        Finished(String),
    }
    let bar_or_info = v.bar() || v.info();
    let (tx, rx) = bar_or_info.then(channel).unzip();
    let buf = stream::iter(packages)
        .map(|mut p| {
            let p_name = p.to_string();
            let tx = if bar_or_info { tx.clone() } else { None };
            let client = client.clone();
            async move {
                if bar_or_info {
                    tx.as_ref()
                        .unwrap()
                        .send(Status::Processing(p_name.clone()))
                        .unwrap();
                }
                p.download(client, cwd).await;
                if modify {
                    p.modify(cwd);
                };
                if bar_or_info {
                    tx.unwrap().send(Status::Finished(p_name.clone())).unwrap();
                }
            }
        })
        .buffer_unordered(PARALLEL);
    // use to test the difference in speed
    // for mut p in packages { p.download(client.clone()).await; if modify { p.modify().unwrap(); }; bar.inc(1); }
    let handler = if bar_or_info {
        Some(thread::spawn(move || {
            let mut running = vec![];
            let rx = rx.unwrap();
            while let Ok(status) = rx.recv() {
                match status {
                    Status::Processing(p) => {
                        running.push(p);
                    }
                    Status::Finished(p) => {
                        running.swap_remove(running.iter().position(|e| e == &p).unwrap());
                        if v.info() {
                            bar.suspend(|| println!("{:>12} {p}", putils::green("Downloaded")));
                        }
                        bar.inc(1);
                    }
                }
                bar.set_message(running.join(", "));
            }
            bar.finish_and_clear();
        }))
    } else {
        None
    };
    buf.for_each(|_| async {}).await; // wait till its done
    drop(tx); // drop the transmitter to break the reciever loop
    if bar_or_info {
        handler.unwrap().join().unwrap();
        println!(
            "{:>12} updated {} package{} in {}",
            putils::green("Finished"),
            HumanCount(p_count),
            if p_count > 0 { "s" } else { "" },
            HumanDuration(BEGIN.elapsed())
        )
    }
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
fn recursive_delete_empty(dir: &Path, cwd: &Path) -> std::io::Result<()> {
    if read_dir(cwd.join(&dir))?.next().is_none() {
        return remove_dir(cwd.join(dir));
    }
    for p in read_dir(&dir)?.filter_map(|e| {
        let e = e.ok()?;
        e.file_type().ok()?.is_dir().then_some(e)
    }) {
        recursive_delete_empty(&cwd.join(dir).join(p.path()), cwd)?;
    }
    Ok(())
}

fn purge(cfg: &mut ConfigFile, v: Verbosity, cwd: &Path) {
    let mut packages = HashSet::new();
    cfg.for_each(|p| {
        if p.is_installed(cwd) {
            packages.insert(p.clone());
        }
    });
    if packages.is_empty() {
        if cfg.packages.is_empty() {
            panic!("No packages configured (modify the \"godot.package\" file to add packages)")
        } else {
            panic!("No packages installed (use \"gpm --update\" to install packages)")
        };
    };
    let p_count = packages.len() as u64;
    let bar;
    if v.bar() {
        bar = putils::bar(p_count);
        bar.set_prefix("Purging");
    } else {
        bar = ProgressBar::hidden();
    }
    let now = Instant::now();
    packages
        .into_iter()
        .progress_with(bar.clone()) // the last steps
        .for_each(|p| {
            bar.set_message(format!("{p}"));
            if v.info() {
                bar.println(format!(
                    "{:>12} {p} ({})",
                    putils::green("Deleting"),
                    p.download_dir(cwd).strip_prefix(cwd).unwrap().display(),
                ));
            }
            p.purge(cwd)
        });

    // run multiple times because the algorithm goes from top to bottom, stupidly.
    for _ in 0..3 {
        if let Err(e) = recursive_delete_empty(&cwd.join("addons"), &cwd) {
            eprintln!("{e}")
        }
    }
    if v.info() {
        println!(
            "{:>12} purge {} package{} in {}",
            putils::green("Finished"),
            HumanCount(p_count),
            if p_count > 0 { "s" } else { "" },
            HumanDuration(now.elapsed())
        )
    }
}

async fn tree(
    cfg: &mut ConfigFile,
    charset: CharSet,
    prefix: PrefixType,
    print_tarballs: bool,
    client: Client,
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
            CharSet::UTF8 => "├──",  // believe it or not, these are quite unlike
            CharSet::ASCII => "|--", // its hard to tell, with ligatures enable
        },
        match charset {
            CharSet::UTF8 => "└──",
            CharSet::ASCII => "`--",
        },
        prefix,
        print_tarballs,
        0,
        &mut count,
        client,
    )
    .await;
    tree.push_str(format!("{} dependencies", HumanCount(count)).as_str());

    #[async_recursion]
    async fn iter(
        packages: &mut Vec<Package>,
        prefix: &str,
        tree: &mut String,
        t: &str,
        l: &str,
        prefix_type: PrefixType,
        print_tarballs: bool,
        depth: u32,
        count: &mut u64,
        client: Client,
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
                    PrefixType::None => name.to_string(),
                }
                .as_str(),
            );
            if print_tarballs {
                tree.push(' ');
                tree.push_str(p.manifest.tarball.as_str());
            }
            tree.push('\n');
            if p.has_deps() {
                iter(
                    &mut p.manifest.dependencies,
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
                    client.clone(),
                )
                .await;
            }
        }
    }
    tree
}

async fn init(mut packages: Vec<Package>, client: Client, cache: Cache, cwd: &Path) -> Result<()> {
    let mut c = ConfigFile::default();
    if packages.is_empty() {
        let mut has_asked = false;
        let mut just_failed = false;
        while {
            if just_failed {
                putils::confirm("Try again?", true)?
            } else if !has_asked {
                putils::confirm("Add a package?", true)?
            } else {
                putils::confirm("Add another package?", true)?
            }
        } {
            has_asked = true;
            let p: ParsedPackage = putils::input("Package?")?;
            let p_name = p.to_string();
            let res = p.into_package(client.clone(), cache.clone()).await;
            if let Err(e) = res {
                putils::fail(format!("{p_name} could not be parsed: {e}").as_str())?;
                just_failed = true;
                continue;
            }
            packages.push(res.unwrap());
        }
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
        println!(
            "{}",
            tree(
                &mut c,
                CharSet::UTF8,
                PrefixType::Indent,
                false,
                client.clone()
            )
            .await
        );
    };

    if !c.packages.is_empty()
        && putils::confirm("Would you like to install your new packages?", true)?
    {
        update(&mut c, true, Verbosity::Normal, client.clone(), cwd).await;
    };
    println!("Goodbye!");
    Ok(())
}

#[cfg(test)]
mod test_utils {
    use glob::glob;
    use sha2::{Digest, Sha256};
    use std::{fs::create_dir, fs::read, path::Path};
    use tempfile::TempDir;

    pub fn mktemp() -> TempDir {
        let tmp_dir = TempDir::new().unwrap();
        create_dir(tmp_dir.path().join("addons")).unwrap();
        tmp_dir
    }

    pub fn hashd(d: &Path) -> Vec<String> {
        let mut files = glob(format!("{}/**/*", d.display()).as_str())
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

#[tokio::test]
async fn gpm() {
    let t = test_utils::mktemp();
    let c = mkclient();
    let cfg_file = &mut config_file::ConfigFile::new(
        &r#"packages: {"@bendn/test":2.0.10}"#.into(),
        c.clone(),
        create_cache(),
    )
    .await;
    update(cfg_file, false, Verbosity::Verbose, c.clone(), t.path()).await;
    assert_eq!(test_utils::hashd(&t.path().join("addons")).join("|"), "1c2fd93634817a9e5f3f22427bb6b487520d48cf3cbf33e93614b055bcbd1329|8e77e3adf577d32c8bc98981f05d40b2eb303271da08bfa7e205d3f27e188bd7|a625595a71b159e33b3d1ee6c13bea9fc4372be426dd067186fe2e614ce76e3c|c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e|c5566e4fbea9cc6dbebd9366b09e523b20870b1d69dc812249fccd766ebce48e|c850a9300388d6da1566c12a389927c3353bf931c4d6ea59b02beb302aac03ea|d060936e5f1e8b1f705066ade6d8c6de90435a91c51f122905a322251a181a5c|d711b57105906669572a0e53b8b726619e3a21463638aeda54e586a320ed0fc5|d794f3cee783779f50f37a53e1d46d9ebbc5ee7b37c36d7b6ee717773b6955cd|e4f9df20b366a114759282209ff14560401e316b0059c1746c979f478e363e87");
    purge(cfg_file, Verbosity::Verbose, t.path());
    assert_eq!(
        test_utils::hashd(&t.path().join("addons")),
        vec![] as Vec<String>
    );
    assert_eq!(
        tree(
            cfg_file,
            crate::CharSet::UTF8,
            crate::PrefixType::Indent,
            false,
            c.clone(),
        )
        .await
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
    use dialoguer::{theme::Theme, Confirm, Input, Select};
    use indicatif::{ProgressBar, ProgressStyle};
    use std::fmt;
    use std::io::Result;
    use std::str::FromStr;

    #[inline]
    pub fn err() -> StyledObject<&'static str> {
        style("Error").red().bold()
    }

    #[inline]
    pub fn select<T: ToString>(items: &[T], p: &str, default: usize) -> Result<usize> {
        Select::with_theme(&BasicTheme::default())
            .items(items)
            .with_prompt(p)
            .default(default)
            .interact()
    }

    #[inline]
    pub fn confirm(p: &str, default: bool) -> Result<bool> {
        Confirm::with_theme(&BasicTheme::default())
            .with_prompt(p)
            .default(default)
            .interact()
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

    pub fn fail(message: &str) -> fmt::Result {
        let mut string = String::from("");
        BasicTheme::default().format_error(&mut string, message)?;
        println!("{string}");
        Ok(())
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
    pub fn warn() -> StyledObject<&'static str> {
        style("Warn").yellow().bold()
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
