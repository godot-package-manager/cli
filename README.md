# Godot Package Manager rust client

[![discord](https://img.shields.io/discord/853476898071117865?label=chat&logo=discord&style=for-the-badge&logoColor=white)](https://discord.gg/6mcdWWBkrr "Chat on Discord")
[![aur](https://img.shields.io/aur/version/godot-package-manager-git?color=informative&logo=archlinux&logoColor=white&style=for-the-badge)](https://aur.archlinux.org/packages/godot-package-manager-git "AUR package")

## Installation

> **Note** read the [using packages quickstart](https://github.com/godot-package-manager#using-packages-quickstart) first.

<details open>
<summary>Manual</summary>

1. Download the [latest release](https://github.com/godot-package-manager/cli/releases/latest/download/godot-package-manager)
2. Move the executable to your `PATH` as `gpm`

</details>
<details>
<summary>ArchLinux</summary>

> **Note** This package installs to /usr/bin/godot-package-manager to avoid conflicts with [general purpose mouse](https://www.nico.schottelius.org/software/gpm/)

1. `pacman -S godot-package-manager-git`

</details>

## Usage

```bash
gpm update # downloads the newest versions of packages
gpm purge # removes the installed packages
gpm tree # prints the tree of installed packages, looks like
# /home/my-package
# └── @bendn/test@2.0.10
#    └── @bendn/gdcli@1.2.5
```

## Compiling

1. `git clone --depth 5 https://github.com/godot-package-manager/client`)
2. `cargo build -r`
3. Executable is `target/release/godot-package-manager`
