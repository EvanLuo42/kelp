# Artifacts

The artifact store keeps large build outputs, such as editor binaries, symbols or packaged builds, next to the changelist they were built from, without adding them to history.
Artifacts share chunk storage with everything else, so consecutive builds that differ a little cost little.

## Publishing

```console
$ kelp artifact publish ./Binaries --name editor-win64 --tag config=development
```

The artifact records the stream and changelist; in a workspace they default to the workspace's stream and the changelist it is synced to.
Pass `--stream` and `--cl` otherwise.

Publishing requires write permission on `<stream>/.artifacts/<name>`, so the protections table can grant it to a CI account alone.

## Finding and fetching

```console
$ kelp artifact list --name editor-win64 --tag config=development
$ kelp artifact get 42 ./out
```

## Placing artifacts in workspaces

A workspace can ask sync to place an artifact, as a read-only directory:

```toml
[[artifact]]
name = "editor-win64"
to = "Engine/Binaries"
tags = { config = "development" }
```

By default sync uses the newest matching artifact built at or below the synced changelist; `exact = true` requires one built from exactly that changelist.
`stream` selects a stream other than the workspace's.
Kelp owns the directory: it is excluded from `status`, and files of a replaced artifact are removed.

## Retention

```console
$ kelp artifact retain --name editor-win64 --keep 5
$ kelp artifact pin 42
$ kelp artifact delete 41
```

These commands require an administrator.
Retention applies at once and after every publish; pinned artifacts are kept regardless.
Garbage collection reclaims the content of deleted artifacts.
