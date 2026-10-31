# Workspaces

A workspace is a directory on your machine bound to one stream.
Kelp keeps its own files in `.kelp/` at the workspace root.

## Creating a workspace

```console
$ kelp init //game/main ws
$ cd ws
$ kelp sync
```

`kelp init` takes the stream and an optional directory, which defaults to the current one.
The workspace gets a name from the directory plus a random suffix; pass `--name` to choose one.
Exclusive locks belong to a user in a named workspace.

Commands run outside a workspace need to know the server.
Pass `--server <host:port>`, or rely on the server you last logged in to.

## The view

The view decides which files of the stream appear in the workspace and where.
It lives in `.kelp/config.toml`:

```toml
name = "ws-7f3a"
server = "kelp.studio:7474"
stream = "//game/main"
view = """
//game/main/...                          ...
-//game/main/Content/Cinematics/...
//game/main/Content/Cinematics/Ep1/...   Cine/Ep1/...
"""
```

Each line maps a depot directory to a workspace directory.
A line starting with `-` excludes a directory.
Later lines override earlier ones.
Only a trailing `...` wildcard is supported.

## Syncing

`kelp sync` brings the workspace to the head of its stream.
It can also go to a changelist or a [label](history.md#labels), and limit itself to some paths:

```console
$ kelp sync
$ kelp sync @120
$ kelp sync Content/Maps @ship-1.0
```

Sync never overwrites a file you have opened.
If an opened file changed on the server, sync schedules a resolve instead; see [Submitting and Resolving](submitting.md).

## Seeing what changed

```console
$ kelp status
$ kelp diff
$ kelp fstat Source/main.rs
```

`status` lists opened files, then files that changed on disk without being opened.
`diff` shows the changes as unified diffs.
`fstat` shows, per file, whether it is opened, modified, locked or out of date.

Files you change outside of Kelp can be opened in one step:

```console
$ kelp reconcile
```

`kelp clean` does the opposite: it puts every unopened file back as synced and deletes files the workspace does not track.
The discarded changes are not kept anywhere.
`kelp clean -n` lists what it would do.

## Ignoring files

`.kelpignore` at the workspace root lists files that are never added.
Each non-empty line not starting with `#` is a glob.
A pattern starting with `/` is anchored at the root; any other pattern matches at any depth.
A pattern also matches everything below a matching directory.

```text
Intermediate/
Saved/
*.tmp
/build.log
```

## kelpd

Workspace commands run in `kelpd`, a per-user daemon that `kelp` starts when needed and that exits after 30 minutes without requests. kelpd watches workspaces so that `status` does not have to scan the disk, and hosts [projected workspaces](projected.md).

`--no-daemon`, or the `KELP_NO_DAEMON` environment variable, runs a command in the `kelp` process instead.
This is useful on build machines.

## Moving a server

When the server moves to a new address, point the workspace at it:

```console
$ kelp relocate kelp.studio:7474
```
