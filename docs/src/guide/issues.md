# Issues

Kelp links changelists to issues in an external tracker and answers where those changes have gone.
It does not store issues themselves.

## Linking

An issue key looks like `PROJ-123`: at least two capital letters, a dash and a number.
Keys mentioned in a changelist's description are linked on submit.
`--fixes` marks the changelist as a fix:

```console
$ kelp submit -m "Fix the double jump (GAME-88)" --fixes GAME-88
```

A depot's typemap can limit which prefixes count:

```toml
[issues]
prefixes = ["GAME", "ART"]
```

The author or an administrator can correct links after the submit:

```console
$ kelp issue link 1043 --add GAME-90 --remove GAME-88
```

## Tracking

```console
$ kelp issue show GAME-88
$ kelp issue in --stream //game/rel-1.0 --since 1100
$ kelp issue in --artifact 42 --since 41
```

`issue show` lists the changelists linked to an issue and the streams that contain them, through merges, copy-ups and stream creation.

`issue in` lists the issues a stream contains, or that a build [artifact](artifacts.md) was built with.
With `--since`, it lists only those added after an earlier changelist or artifact: a changelog, or the answer to "is the fix in tonight's build?".

Submit events sent to [webhooks](../admin/webhooks.md) carry the linked issues, so a tracker can update itself.
