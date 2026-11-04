# Changelog

All notable changes to Kelp are recorded here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Kelp uses [Semantic Versioning](https://semver.org/).

When releasing, rename `Unreleased` to the new version and date.
The release workflow publishes the section of the tagged version as the GitHub Release notes.

## [Unreleased]

### Added

- A display name and any number of email addresses per user, set with `kelp user name`, `add-email`, `remove-email` and `primary-email`.
  `changes`, `describe`, `annotate`, reviews and issues show the display name instead of the account name.
- Depot administrators: the `admin` level in the protections table lets its users create and delete streams, obliterate, force-unlock and manage typemaps, imports and artifacts where the line applies.
- Partial administrative roles, granted with `role <role> <who>` lines: `accounts`, `operations`, `audit` and `integrations`.
- `kelp shelves` lists shelves, newest first, optionally of one stream or user.
- `kelp diff` shows files whose type names a `differ` plugin as the plugin describes them.
- `kelp diff --tool` and `kelp snapshot diff --tool`, or `KELP_DIFF_TOOL`, open each file in an external diff tool.

### Changed

- The metadata format is now version 10, which adds user profiles and an index of where content appears; existing databases migrate on open.
- Messages larger than 16 MiB, such as the submit of hundreds of thousands of files, are split across frames, up to 1 GiB, instead of failing.
- A merge that leaves out a change of its source, for example because the workspace's view does not cover it, is refused.

### Fixed

- Fetching content by its hash now requires read permission on a path it appears at; clients from before this release keep working for users who may read every path.
- `status` no longer misses a change that keeps a file's size within the same tick of a coarse file system clock, such as FAT's, or of a file server whose clock differs from the client's.

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
