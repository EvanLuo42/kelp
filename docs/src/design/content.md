# Content Storage

## Chunks and manifests

Files are split into chunks by FastCDC, a content-defined chunker: chunk boundaries depend on the content around them, so an insertion changes only the chunks it touches.
The chunk sizes are 16 KiB minimum, 64 KiB average and 256 KiB maximum, and four times that for files over 64 MiB.

A chunk is identified by the BLAKE3 hash of its uncompressed bytes and stored compressed with zstd.
A file's *manifest* lists its chunks.
A file is identified by its *content id*, the BLAKE3 hash of the whole file.

Because a file's identity does not depend on how it was chunked, different chunkers, including plugins, can coexist.
They change how well content deduplicates, never whether it is correct.

## Blob stores

| Store | Used for |
|---|---|
| pack store | local storage: small chunks appended to pack files, large chunks one file each |
| object store | S3-compatible buckets, optionally under a prefix |
| tiered store | the pack store for current content, a bucket for history |

Every store's `put` is idempotent, so retried uploads are harmless.

## Garbage collection

GC is mark and sweep, not reference counting, which is fragile with deduplication across streams and concurrent uploads.
The live set is the union of every unpurged revision, every shelf, the merge bases of streams, every artifact and every registered plugin, plus *upload leases*: content a client uploaded or asked about in the last hour, which protects submits in progress.
Leases live in memory, so GC waits an hour after a start.
Trees submitted while GC marks are checked again before the sweep.
Pack files with many dead chunks are rewritten afterwards.
