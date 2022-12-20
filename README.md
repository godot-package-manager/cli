# Godot Package Manager rust client

[![discord](https://img.shields.io/discord/853476898071117865?label=chat&logo=discord&style=for-the-badge&logoColor=white)](https://discord.gg/6mcdWWBkrr "Chat on Discord")
[![aur](https://img.shields.io/aur/version/godot-package-manager-git?color=informative&logo=archlinux&logoColor=white&style=for-the-badge)](https://aur.archlinux.org/packages/godot-package-manager-git "AUR package")

## Installation

> **Note** read the [using packages quickstart](https://github.com/godot-package-manager#using-packages-quickstart) first.

<details open>
<summary>Manual</summary>

1. Clone this repo (`git clone --depth 1 https://github.com/godot-package-manager/client`)
2. Compile (`cargo build -r`)
3. Put the executable in your `PATH` (`mv target/godot-package-manager /usr/bin`)

</details>
<details>
<summary>ArchLinux</summary>

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
