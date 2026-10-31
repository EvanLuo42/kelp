# Projected Workspaces

A projected workspace shows every file as soon as it is synced and downloads a file's content the first time a program opens it.
Opening a project with millions of files then takes seconds, and disk use grows only with what you touch.

```console
$ kelp init //game/main --projected ws
$ cd ws
$ kelp sync
```

Projected workspaces are served by kelpd, which mounts them again when it starts.
`kelp unmount` unmounts the workspace in the current directory until kelpd mounts it again.

| Platform | Backend |
|---|---|
| Linux | FUSE |
| Windows | ProjFS |

Listing a directory and reading file sizes never downloads content.
A projected file's modification time is the time it was projected, so build tools that compare timestamps see it as new.

Everything else works as in an ordinary workspace: opening, submitting, resolving and status.
Artifacts placed by sync are written in full.
