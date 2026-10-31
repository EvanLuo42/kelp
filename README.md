# Kelp

Kelp is a centralized version control system for game development.
It handles engine code, game code and large binary assets, and is engine-agnostic.

- Changelists, streams, exclusive locks and path permissions
- Content-defined chunking with deduplication across all history
- Multiple pending changelists, hunk-level splitting, shelves and snapshots
- Projected workspaces that download files on first use
- One binary from a single server to edges and a Raft commit group on S3
- Built-in accounts, two-factor login and OpenID Connect

## Installation

```console
$ cargo install kelp-server
$ cargo install kelp-cli
```

## Quick start

```console
$ kelp-server --data ./kelp-data init
$ kelp-server --data ./kelp-data serve --listen 127.0.0.1:7474
```

In another terminal:

```console
$ kelp login 127.0.0.1:7474
$ kelp depot create game
$ mkdir ws && cd ws
$ kelp init //game/main
$ kelp sync
```

## Documentation

[The Kelp Book](https://evanluo42.github.io/kelp/) covers usage, administration and design.
Its source is in `docs/`; build it locally with `mdbook build docs`.

## Contributing

`cargo test --workspace --all-features` runs the tests.
Tests live in each crate's `tests/` directory and reach the file system, clock and network only through the fixtures in `kelp-testkit`.
Doc comments follow the style of the Rust standard library.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
