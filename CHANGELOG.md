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
- `[oidc] expect_claim` chooses which claim links a first single sign-on login to an account an administrator expected (`sub`, verified `email`, or the user name as before).

### Changed

- The metadata format is now version 10; existing databases migrate on open.
  It adds user profiles and an index of where content appears, folds names one letter at a time as NTFS and APFS do (so a final `ς` and `σ` are one name), drops views left by deleted streams and records stream owners.
- The wire protocol is now version 2, because user profiles changed several messages.
  Kelp 0.1.0 clients, `kelpd`, edges and commit group members are refused with an error that says which side to upgrade; see [Upgrading](docs/src/admin/upgrading.md).
- Workspace state is now version 3; existing workspaces upgrade on open.
- Messages larger than 16 MiB, such as the submit of hundreds of thousands of files, are split across frames, up to 1 GiB, instead of failing.
- A merge that leaves out a change of its source, for example because the workspace's view does not cover it, is refused.
- Only its owner or an administrator of it may delete a task stream, and others may create a task stream only from a parent whose every file they can read.
- Turning on two-factor login again requires turning it off first, with a current code.
- Failed logins no longer reveal whether an account exists.

### Fixed

- Fetching content by its hash now requires read permission on a path it appears at.
- Tree diffs no longer accept trees from outside the named stream, which let a user read files hidden from them.
- Sessions end when their token is revoked or expires, and follow a merged user.
- Edges bound uploads they forward and store only manifests verified by the commit server.
- Garbage collection no longer frees content that a concurrent submit, shelve or obliterate refers to.
- A merge into a parent from a child no longer marks the child as having merged the parent.
- `filelog` follows a move into a path that existed before.
- Obliterate also removes paths changed through writable imports.
- `status` no longer misses a change that keeps a file's size within the same tick of a coarse file system clock, such as FAT's, or of a file server whose clock differs from the client's.
- Workspaces no longer report tracked files that became ignored as deleted, snapshot files opened for delete before overwriting them, and restore the moves of a snapshot.
- Sync checks each file against its content id, skips paths inside `.kelp` directories, and skips a file whose directory is blocked by a file instead of failing.
- The credentials file is created private, and the CLI refuses a `kelpd` pipe served by another Windows user.
- `kelpd` of another version can be stopped, two `kelpd` no longer start for one home on Unix, and accept errors no longer stop it.
- Projected workspaces keep files open for writing through a sync, keep inodes across renames, and hydrate ProjFS placeholders with their own version.
- Caches replace corrupt chunks, and a failed pack fsync keeps failing until the lost chunks are stored again.
- Plugins are limited to one memory and bounded tables, and file reads are capped and cost fuel.
- `--password-stdin` refuses end of input and empty passwords.

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
