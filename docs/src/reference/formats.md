# Text Formats

Several settings are line-based text documents.
In all of them, blank lines and lines starting with `#` are ignored, and fields are separated by whitespace.

## Views

```text
//game/main/...                          ...
-//game/main/Content/Cinematics/...
//game/main/Content/Cinematics/Ep1/...   Cine/Ep1/...
```

`<depot directory>/... <workspace directory>/...`, or `-<depot directory>/...` to exclude.
Later lines override earlier ones.
Only a trailing `...` is supported.

## .kelpignore

One glob per line.
A pattern with a `/` at the start or in the middle is anchored at the workspace root; otherwise it matches at any depth.
A trailing `/` makes the pattern match directories only.
A pattern also matches everything below a matching directory.
A leading `!` negates the pattern: it re-includes paths ignored by earlier lines, and the last matching line wins.
A file cannot be re-included when a parent directory is ignored as a whole.
`\!` and `\#` match names that start with `!` or `#`.

## Protections

```text
<level> <who> <path>
```

| Field | Values |
|---|---|
| level | `none`, `read`, `write` |
| who | `*`, `user:<name>`, `group:<name>` |
| path | a depot path; `/...` at the end covers a directory |

The last line that matches a user and a path wins.

## Review rules

```text
approve <count> <who> <path>
status <check> <path> [by <who>]
issue <path>
```

`who` is as in protections.
Paths are exact or end in `/...`.

## Imports

```text
<source stream> <from> <to> [at=<changelist>] [writable]
```

`from` is a directory of the source stream, or `.` for all of it.
`to` is a directory of the importing stream.
