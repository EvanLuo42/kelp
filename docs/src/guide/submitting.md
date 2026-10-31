# Submitting and Resolving

## Submitting

```console
$ kelp submit -c L1 -m "Fix the jump height"
Submitted changelist 1043.
```

Without `-c`, `submit` submits the default changelist.
Without `-m`, it uses the changelist's description.

A submit is atomic: every file in it is committed, or none is.
The server rejects it if:

- a file changed on the server since you synced it,
- an exclusive file is not locked by you,
- you lack write permission on a path,
- a [review](reviews.md) gate is not satisfied, or
- a [trigger plugin](../admin/plugins.md) rejects it.

The first case is the common one.
Sync, resolve, and submit again.

Kelp detects concurrent changes by comparing content: a file whose content on the server equals the content you started from can be submitted.

## Resolving

When sync finds that a file you have opened changed on the server, it does not overwrite your version.
It schedules a resolve.
A changelist with unresolved files cannot be submitted.

```console
$ kelp sync
$ kelp resolve
$ kelp resolve --accept yours Content/Hero.uasset
$ kelp resolve --accept theirs Config/Game.ini
```

Without `--accept`, `resolve` merges text files three ways.
A clean merge is written to the file.
A merge with conflicts writes conflict markers; edit the file, then run `kelp resolve --accept yours` on it.
Binary files need an explicit choice, unless the typemap names a merge plugin for them.

## Undoing a changelist

`kelp undo` opens the reverse of a submitted changelist in a new changelist, which you then submit:

```console
$ kelp undo 1043
$ kelp submit -m "Back out 1043"
```

The workspace must be synced to the head first.
