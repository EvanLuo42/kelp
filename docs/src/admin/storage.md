# Storage

Files are stored as chunks, deduplicated across all streams and history.
Where chunks live is set in `server.toml`:

```toml
[storage]
kind = "tiered"

[storage.s3]
bucket = "studio-kelp"
region = "eu-central-1"
```

| Kind | Chunks live in |
|---|---|
| `local` | the data directory; the default |
| `s3` | an S3-compatible bucket |
| `tiered` | the data directory for current content, the bucket for history |

S3 credentials come from the usual environment variables, such as `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
`endpoint` selects an S3-compatible service such as MinIO or R2, and `prefix` lets several servers share a bucket.

With tiered storage, `kelp tier` moves chunks that no stream's current files use to the bucket.
Reads look in the bucket when a chunk is not local.

## Garbage collection

Content becomes garbage when nothing refers to it: after obliterate, after retention purges old revisions, or when shelves and artifacts are deleted.

```console
$ kelp gc --dry-run
$ kelp gc
```

GC never deletes content uploaded in the last hour, so submits in progress are safe.
After the server starts, GC waits an hour for the same reason.

## Retention

A typemap rule with `retain = N` keeps the content of only the newest N revisions of matching files.
Older revisions stay in history, marked purged, and their content is collected by GC.

## Obliterate

```console
$ kelp obliterate //game/main/Content/Leaked/... --yes
```

Obliterate removes files and all their history in every stream, as if they had never been submitted.
Use it for content that must not be kept, such as leaked credentials or licensed material.
The obliterate itself stays in the audit log, with paths but no content.

## The audit log

Every command the server applies is recorded with its author in a hash chain.

```console
$ kelp audit
$ kelp audit --verify
```

`--verify` checks the whole chain against the command log.
