# Installation

Kelp is distributed as two crates on crates.io:

| Crate | Binaries | Install on |
|---|---|---|
| `kelp-server` | `kelp-server` | the server machine |
| `kelp-cli` | `kelp`, `kelpd` | every client machine |

## Prebuilt binaries

Each [GitHub Release](https://github.com/EvanLuo42/kelp/releases) has release builds for Linux and Windows (x86_64), with a `SHA256SUMS` file.
The installation scripts download the latest one, verify its checksum and put the binaries in a directory on your `PATH`.

```console
$ curl -fsSL https://raw.githubusercontent.com/EvanLuo42/kelp/main/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/EvanLuo42/kelp/main/install.ps1 | iex
```

In a terminal the scripts ask what to install (everything, only the client, or only the server) and where.
Without a terminal they install everything into `~/.local/bin` on Linux and `%USERPROFILE%\.kelp\bin` on Windows.
Options, or the environment variable next to each of them, skip the questions:

| Option | Environment variable | |
|---|---|---|
| `--components all\|cli\|server` | `KELP_COMPONENTS` | what to install |
| `--dir <path>` | `KELP_INSTALL_DIR` | installation directory |
| `--version <tag>` | `KELP_VERSION` | a release such as `v0.1.1`, or `nightly`, instead of the latest |
| `--no-modify-path` | `KELP_NO_MODIFY_PATH=1` | do not add the directory to `PATH` |
| `--yes` | `KELP_YES=1` | do not ask questions |

On Windows the options are `-Components`, `-InstallDir`, `-Version`, `-NoModifyPath` and `-Yes`, which need the script downloaded and run as a file; through `iex` use the environment variables.

```console
$ curl -fsSL https://raw.githubusercontent.com/EvanLuo42/kelp/main/install.sh | sh -s -- --components cli --dir ~/bin
```

On Linux the script adds the directory to `PATH` in `~/.profile` and the profile of your shell; on Windows it adds it to the user `PATH`.
The Linux binaries are built on a recent Ubuntu and need a matching glibc.
Elsewhere, install with Cargo.

## Nightly builds

`--version nightly` installs the newest commit of `main` that passed CI.
A scheduled workflow builds it once a day, when `main` has changed, and replaces the files of the `nightly` pre-release on GitHub.
`kelp --version` shows the commit, for example `0.1.0+nightly.20261101.5b6d901`.
Nightly builds are not reviewed or announced, and may break or lose data; do not use them on a server with data you need.
Metadata is migrated forward when a database is opened, so going back to a release may not work.

## With Cargo

Kelp requires Rust 1.97 or later.

```console
$ cargo install kelp-server
$ cargo install kelp-cli
```

`kelp` starts `kelpd` from the directory it is installed in, so keep both binaries together.

## Platform support

Kelp is tested on Linux and Windows.
`kelp-server service`, which installs a systemd unit, is Linux only.

Projected workspaces use FUSE on Linux and ProjFS on Windows.
On Linux, mounting needs `/dev/fuse` and the `fusermount3` or `fusermount` helper, from the `fuse3` or `fuse` package; libfuse itself is not needed.
On Windows, enable the *Windows Projected File System* optional feature.
Ordinary workspaces need neither.

## Building from source

```console
$ git clone https://github.com/EvanLuo42/kelp
$ cd kelp
$ cargo build --release
```

The binaries are in `target/release`.
