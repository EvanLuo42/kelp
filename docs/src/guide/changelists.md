# Changelists

A pending changelist groups opened files you intend to submit together.
Each workspace has a default changelist and any number of numbered ones, named `L1`, `L2` and so on.

## Opening files

```console
$ kelp add Source/new.rs
$ kelp edit Content/Hero.uasset
$ kelp delete Source/old.rs
$ kelp move Source/a.rs Source/b.rs
```

Files open in the default changelist unless you pass `-c <changelist>`.
`edit` takes an exclusive lock on files whose type requires one; see [Locks and the Typemap](typemap.md).
`move` renames a file, or every file below a directory, and keeps their history.

`kelp revert <paths>` discards your changes to opened files and closes them.

## Managing changelists

```console
$ kelp cl new "Fix the jump height"
Created changelist L1.
$ kelp cl move L1 Source/player.rs
$ kelp cl list
$ kelp cl delete L1
```

Deleting a changelist moves its files to the default changelist.

## Splitting a file by hunk

A text file can have its hunks in different changelists.
List the hunks, then move one:

```console
$ kelp cl split Source/player.rs
$ kelp cl split Source/player.rs --hunk 2 --to L1
```

Submitting `L1` submits only the hunks it owns, applied to the version you synced.
The file on disk does not change, and the other hunks stay open in their changelists.
Binary files and exclusive files cannot be split.

## Snapshots

A snapshot saves the opened files of a changelist in your local content cache.
It works offline, like a private commit.

```console
$ kelp snapshot save -c L1 -m "before refactoring"
$ kelp snapshot list
$ kelp snapshot diff 3
$ kelp snapshot restore 3
```

`snapshot diff` compares two snapshots, or one snapshot with the disk.
`restore` writes the files back and reopens them as they were.
`revert` and `resolve` take a snapshot before they overwrite anything.

## Shelves

A shelf stores a pending changelist on the server, where others can see it.

```console
$ kelp shelve -c L1
Shelved as 42.
$ kelp unshelve 42
$ kelp delete-shelf 42
```

`kelp shelves` lists shelves, newest first, limited to one stream with `--stream` or one user's with `--user`:

```console
$ kelp shelves --user alice
shelf 42     //game/main by alice@alice-pc (3 files) Fix the bridge collision
```

A shelf takes a changelist number from the same sequence as submits.
Shelving the same changelist again replaces the shelf and keeps its number.
`unshelve` requires the files to be at the versions the shelf was based on, and opens them in a new changelist, or in the one given with `-c`.
`unshelve --sync` first syncs just the shelf's files to those versions, so a clean workspace is enough.
Shelves are also what [reviews](reviews.md) are based on.
