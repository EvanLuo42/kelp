# History

## Changelists

```console
$ kelp changes
$ kelp changes --stream //game/dev -n 50
$ kelp describe 1043
```

`changes` lists recent submitted changelists of the workspace's stream.
`describe` shows one changelist: its author, description, linked issues and files.

## Files

```console
$ kelp filelog Source/player.rs
$ kelp annotate Source/player.rs
$ kelp print Source/player.rs@1040
$ kelp print //engine/main/Core/log.h
```

`filelog` lists the revisions of a file.
History follows moves: the revisions a file had under its old name are listed too.

`annotate` prints each line of a text file with the changelist and author that last changed it.

`print` writes a file as of a changelist or label to standard output.
Both workspace paths and depot paths work.

## Labels

A label names a stream as of a changelist.

```console
$ kelp label set ship-1.0 --cl 1043 -m "First release"
$ kelp label list
$ kelp sync @ship-1.0
$ kelp label delete ship-1.0
```

Without `--cl`, the label names the head of the stream.
Setting an existing label moves it.
Wherever a changelist can be given as `@<number>`, a label can be given as `@<name>`.
