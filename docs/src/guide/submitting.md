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

### Merge tools

To settle conflicts in a program of your own, give it with `--tool`, or set `KELP_MERGE_TOOL` to use it every time:

```console
$ kelp resolve --tool "meld {yours} {base} {theirs} --output {merged}"
$ kelp resolve --tool "code.cmd --wait --merge {theirs} {yours} {base} {merged}"
```

`resolve` still merges first.
Only files it cannot settle, those with conflicts and those that need a choice, are opened in the tool, one after another.

`{base}`, `{yours}` and `{theirs}` stand for files with the three versions, which keep the file's name, and `{path}` for its depot path.
`{merged}` stands for the file the tool writes the result to.
It starts out as the workspace file, with conflict markers if the merge had conflicts.
Without any of the four files, `{base} {yours} {theirs} {merged}` are added to the end of the command.
A version that does not exist, such as the base of a file both sides added, is an empty file.
The tool runs once per file, without a shell; quote words that contain spaces.

The result is taken if the tool exits with 0.
If it does not, or the result still has conflict markers, the file stays as it was and keeps its pending resolve.
A file that one side deletes is not opened in the tool; use `--accept`.
`--tool` cannot be combined with `--accept`, and `KELP_MERGE_TOOL` is ignored when `--accept` is given.
On Windows, name `.cmd` and `.bat` programs with their extension, as in `code.cmd`.

## Undoing a changelist

`kelp undo` opens the reverse of a submitted changelist in a new changelist, which you then submit:

```console
$ kelp undo 1043
$ kelp submit -m "Back out 1043"
```

The workspace must be synced to the head first.
