# Installation

Kelp is distributed as two crates on crates.io:

| Crate | Binaries | Install on |
|---|---|---|
| `kelp-server` | `kelp-server` | the server machine |
| `kelp-cli` | `kelp`, `kelpd` | every client machine |

Install them with Cargo.
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
