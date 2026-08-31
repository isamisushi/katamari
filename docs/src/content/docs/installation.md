---
title: Installation
description: Every way to install katamari — Homebrew, the install script, prebuilt binaries, mise, from source, and Windows via WSL.
---

Every method installs two identical binaries, `katamari` and `ktmr` (the
short name is what the rest of this document uses). There are no distro
packages (apt/dnf/pacman) yet — the channels below are the complete list.

## Homebrew (macOS and Linux)

```
brew install isamisushi/tap/katamari
```

## Install script (macOS and Linux)

Detects your platform — on Linux, choosing between the glibc and static
musl builds — and installs without needing a Rust toolchain:

```
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/isamisushi/katamari/releases/latest/download/katamari-installer.sh | sh
```

## Prebuilt binaries (macOS and Linux)

Each [release](https://github.com/isamisushi/katamari/releases) ships a
`tar.xz` per target, each with a matching `.sha256` alongside it:

| OS    | CPU                     | archive                                   |
| ----- | ----------------------- | ----------------------------------------- |
| macOS | Apple Silicon           | `katamari-aarch64-apple-darwin.tar.xz`    |
| macOS | Intel                   | `katamari-x86_64-apple-darwin.tar.xz`     |
| Linux | x86_64 (glibc)          | `katamari-x86_64-unknown-linux-gnu.tar.xz` |
| Linux | aarch64 (glibc)         | `katamari-aarch64-unknown-linux-gnu.tar.xz` |
| Linux | x86_64 (musl, static)   | `katamari-x86_64-unknown-linux-musl.tar.xz` |
| Linux | aarch64 (musl, static)  | `katamari-aarch64-unknown-linux-musl.tar.xz` |

Download, extract, and put the binaries anywhere on `$PATH` — the same
three commands work on both OSes (shown for Linux x86_64; substitute your
archive name from the table):

```
curl -LO https://github.com/isamisushi/katamari/releases/latest/download/katamari-x86_64-unknown-linux-gnu.tar.xz
tar -xJf katamari-x86_64-unknown-linux-gnu.tar.xz
sudo install katamari-x86_64-unknown-linux-gnu/ktmr katamari-x86_64-unknown-linux-gnu/katamari /usr/local/bin/
```

The gnu archives need glibc 2.34 or newer — in distro terms, Ubuntu
22.04, Debian 12, RHEL/Rocky/Alma 9, or anything more recent (`ldd
--version` prints yours). On anything older, and on musl-based distros
like Alpine, use the musl archives: fully static, they run on any Linux.

## mise

If you already use [mise](https://mise.jdx.dev/), its `ubi` backend
installs `ktmr` straight from the same release archives, on any of the
targets above:

```
mise use -g "ubi:isamisushi/katamari[exe=ktmr]"
```

## From source

Any OS with a Rust toolchain (any recent stable) — and the route for
targets without prebuilt archives, like the BSDs:

```
git clone https://github.com/isamisushi/katamari.git
cd katamari
cargo install --path .
```

If this repository itself is managed with [mise](https://mise.jdx.dev/),
`mise.toml` already pins a Rust toolchain and defines `mise run
build`/`test`/`lint`/`fmt` tasks for working on katamari's own source;
none of that is required to build or run it.

## Windows

No prebuilt binaries, and katamari isn't tested natively on Windows — run
it inside [WSL](https://learn.microsoft.com/windows/wsl/) and follow the
Linux instructions there. (A native `cargo install` may compile, but parts
of the tool — `ktmr skill install`'s symlinks (`--user` included), LSP
auto-install's executable-bit handling — assume a Unix filesystem.)

## Updating

The right command depends on which method above you used. A session's
on-quit notice (`vX.Y.Z is available — ...`) recognizes the install script,
Homebrew, and `cargo install`, and names the matching command straight away;
for anything else — including mise — it falls back to pointing at the
[release page](https://github.com/isamisushi/katamari/releases), so a mise
install should use `mise upgrade` regardless of what the notice says.

**Install script**: the installer leaves behind a receipt recording where
and how it installed katamari; `ktmr self-update` reads it and re-runs
itself at the latest release.

```
ktmr self-update
```

**Homebrew**:

```
brew upgrade katamari
```

**mise**:

```
mise upgrade
```

**cargo**, or any other install with no receipt to read (prebuilt binaries
installed by hand, a `cargo install --path .` from-source build): `ktmr
self-update` has nothing to work from for these and says so, pointing at
the right command instead — for a `cargo install`-managed binary, that's

```
cargo install --git https://github.com/isamisushi/katamari
```

and for a hand-installed prebuilt binary, downloading the new
[release](https://github.com/isamisushi/katamari/releases) archive and
replacing it in place.
