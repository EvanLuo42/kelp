# Changelog

All notable changes to Kelp are recorded here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Kelp uses [Semantic Versioning](https://semver.org/).

When releasing, rename `Unreleased` to the new version and date.
The release workflow publishes the section of the tagged version as the GitHub Release notes.

## [Unreleased]

## [0.1.0] - 2026-10-31

The first release.

### Added

- An all-in-one server with built-in accounts, two-factor login and OpenID Connect single sign-on.
- Workspaces with multiple pending changelists, hunk-level splitting, local snapshots, shelves and three-way resolve.
- Streams with merge-down, copy-up, cherry-pick, undo and cross-depot imports.
- History: `changes`, `describe`, `filelog` across moves, `annotate`, `print`, `diff` and labels.
- Exclusive locks, typemaps, path permissions and review gates.
- Projected workspaces on Linux (FUSE) and Windows (ProjFS).
- An artifact store for build outputs, placed in workspaces by sync.
- Local, S3 and tiered chunk storage, garbage collection, retention, obliterate and a hash-chained audit log.
- Edge servers and Raft commit groups.
- Webhooks, WebAssembly plugins and a C SDK.
- Backup, restore, `fsck`, systemd service installation, and `/healthz` and Prometheus `/metrics` endpoints.

[Unreleased]: https://github.com/EvanLuo42/kelp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/EvanLuo42/kelp/releases/tag/v0.1.0
