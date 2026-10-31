# Introduction

Kelp is a centralized version control system for game development.
It is built for teams that keep engine source code, game code and large binary assets in one place.

Kelp is engine-agnostic.
It does not read asset formats; behavior that depends on a format, such as smarter chunking or merging, comes from optional WebAssembly plugins.

## What Kelp provides

- **A central server** with globally increasing changelist numbers, exclusive locks for files that cannot be merged, and path-level permissions.
- **Large files as a first-class case.**
  Files are split into content-defined chunks, deduplicated across all history and streams, and transferred incrementally.
- **Multiple pending changelists** per workspace.
  Text files can be split by hunk across changelists.
  Local snapshots and server-side shelves keep work in progress safe.
- **Projected workspaces** that show every file immediately and download content on first use.
- **One binary for every scale**: a single all-in-one server, edge servers in each office, and a Raft commit group on shared object storage.
  Growing from one to the next needs no data migration.
- **Built-in accounts** with two-factor login, and OpenID Connect single sign-on.

## How this book is organized

[Getting Started](installation.md) installs Kelp and walks through a first submit.
The [User Guide](guide/workspaces.md) covers daily work in a workspace.
[Administration](admin/server.md) covers running and growing a server.
The [Reference](reference/config.md) lists file formats and commands, and [Design](design/architecture.md) explains how Kelp works inside.

Every command has `--help`, which is the authoritative description of its options.
