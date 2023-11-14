# Godot Package Manager rust client

[![discord](https://img.shields.io/discord/853476898071117865?label=chat&logo=discord&style=for-the-badge&logoColor=white)](https://discord.gg/6mcdWWBkrr "Chat on Discord")

## Installation

> **Note** read the [using packages quickstart](https://github.com/godot-package-manager#using-packages-quickstart) first.

1. `cargo install gpm`

## Usage

```bash
gpm update # downloads the newest versions of packages
gpm purge # removes the installed packages
gpm tree # prints the tree of installed packages, looks like
# /home/my-package
# └── @bendn/test@2.0.10
#    └── @bendn/gdcli@1.2.5
```
