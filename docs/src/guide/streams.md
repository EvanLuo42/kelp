# Streams

A stream is a branch inside a depot, such as `//game/main` or `//game/dev`.
Streams form a tree: every stream except a mainline has a parent.

| Kind | Purpose |
|---|---|
| mainline | The root of a depot. Created with the depot. |
| development | Work that merges down from its parent and copies up into it. |
| release | Stabilization. Merges down from its parent but never copies up. Has its own locks. |
| task | Short-lived work, such as one bug fix. Can be deleted by anyone who can see it. |

## Creating a stream

```console
$ kelp stream create //game/dev --parent //game/main
$ kelp stream create //game/rel-1.0 --parent //game/main --kind release
$ kelp streams
```

Creating a stream takes constant time; the new stream starts as a reference to its parent's current tree.
Creating development and release streams requires an administrator.
Anyone who can read every file of the parent can create a task stream of it.
A task stream starts as a copy of the parent under its own path, where protections written for the parent's paths do not apply, so a user that some files of the parent are hidden from has to ask an administrator.

## Merging down

In a workspace of the child stream, synced to its head:

```console
$ kelp merge
$ kelp resolve
$ kelp submit -m "Merge main"
```

`merge` opens every file the parent changed since the last merge in a new changelist.
Files changed on both sides are scheduled for resolve.
Submitting the changelist records the merge, so the next merge starts from there.
The server refuses the submit if it leaves out a change of the parent, for example because the workspace's view does not cover a changed file; merge in a workspace that covers it.
Files you resolved are taken as you resolved them.

`--from <stream>` merges from a stream other than the parent.

## Cherry-picking

`merge --cl` picks the changes of one changelist only:

```console
$ kelp merge --cl 1043
$ kelp merge --cl 1043 --from //game/dev
```

## Copying up

When a development stream has merged everything from its parent, its content can be copied into the parent in one step:

```console
$ kelp copy-up -m "Land the new inventory"
```

The server performs the copy in a single transaction and refuses it if the child has not merged the parent's latest changelist, or if a changed exclusive file is locked by someone else.

## Deleting a stream

```console
$ kelp stream delete //game/fix-1234 --yes
```

Deleting a stream keeps its history, which `describe` and `filelog` still show; the path can not be reused.
Administrators can delete any stream but a mainline.
Other users can delete task streams.

## Imports

A stream can mount a directory of a stream in another depot, for example an engine inside a game:

```text
//engine/main  .       Engine  at=18234
//tools/main   Python  Tools   writable
```

Each line is `<source stream> <from> <to> [at=<changelist>] [writable]`.
`from` is a directory of the source stream, or `.` for all of it; `to` is where it appears in this stream.
An administrator sets the list:

```console
$ kelp stream imports set imports.txt --stream //game/main
$ kelp stream imports get --stream //game/main
```

Workspaces see imported files as ordinary files.
Changes under a read-only import are rejected on submit.
Changes under a writable import are submitted to the source stream, in the same changelist as the changes to this stream.
A writable import always follows the source's head; `at=` pins a read-only import to a changelist.

Permissions, the typemap and locks of an imported file are those of its source path, so the same file is locked for everyone in both depots.

Imports do not nest: a stream that imports cannot be imported, and the other way round.
