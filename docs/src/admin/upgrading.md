# Upgrading

Upgrading `kelp-server` means replacing the binary and restarting the server.
There is no separate migration command: the server brings the data directory up to date when it starts.

## What changes between versions

The metadata database (`meta.redb`) and the local chunk pack index record a format version.
On start, a newer server migrates an older format in a single transaction, so a crash during the migration leaves the database in its old format, and the next start tries again.
An older server refuses a directory that a newer version has written, with an error that ends in `upgrade Kelp`.
There is no downgrade: once a newer server has started on a data directory, only a backup brings the old version back.

### Metadata format 10

Format 10 compares names by folding them one letter at a time, as NTFS and APFS do, so `ΟΔΟΣ`, `οδος` and `οδοσ` (and other letter variants such as `µ` and `μ`) are one file.
Earlier formats gave a word-final sigma a key of its own.
The migration re-keys the file trees and the tables keyed by paths; it rewrites tree nodes only if some name is affected, so on most servers it only reads them once.
If two existing names only now fold alike, both are kept: the one already spelled the new way, or else the first, takes the new key, and the server logs a warning naming the other, which keeps its old key and should be renamed.
The migration also drops the views that deleted streams left behind, and records from the command log who created each stream; streams whose creation the log does not show have no recorded owner.

Clients and servers agree on a wire protocol version when they connect, so clients do not have to be upgraded at the same time as the server.
The [changelog](https://github.com/EvanLuo42/kelp/blob/main/CHANGELOG.md) notes anything a release asks of clients.

## A single server

1. Read the release's entry in the changelog.
2. Take a backup, so you can go back if the new version misbehaves:

   ```console
   $ kelp backup /srv/backups/kelp-before-0.2.0
   ```

   See [Backup and Recovery](backup.md).
3. Stop the server.
4. Replace the `kelp-server` binary.
5. Start the server and check the version it prints:

   ```console
   $ kelp-server --data /srv/kelp serve
   2026-11-02T09:00:00.000000Z  INFO kelp_server: kelp-server started version="0.2.0" listen=[::]:7474 ...
   ```

6. Check that the data is intact:

   ```console
   $ kelp fsck
   ```

If the server was installed with `service install`, the unit runs the binary from the path it had when the unit was installed.
Replace the file at that path and restart the unit:

```console
$ sudo systemctl stop kelp-server
$ sudo install -m 755 kelp-server /usr/local/bin/kelp-server
$ sudo systemctl start kelp-server
$ kelp-server --data /srv/kelp service status
```

If the new binary lives somewhere else, run `service install` again from the new binary so the unit points at it.

## Edge servers

An edge only replicates from a commit server with the same database format.
If the formats differ, the edge stops replicating and refuses requests until it is restarted with a matching version; it never serves stale or mis-read data.

Upgrade the commit server first, then every edge:

1. Back up and upgrade the commit server as above.
2. On each edge, stop it, replace the binary, and start it again.
   The edge migrates its own database and resumes replication where it left off.

Clients of an edge cannot read or write through it between the commit server's restart and the edge's, so upgrade the edges right after the commit server.

## Commit groups

Members of a [commit group](commit-groups.md) exchange metadata log entries and snapshots in the database's own encoding, so every member must run the same version.
A member refuses messages from a member with another database format and logs `run the same Kelp version on every member`, so a group with mixed versions loses those members rather than misreading their data.
Do not upgrade one member at a time.

Each member's Raft log (`raft.redb`) holds entries in the old encoding, which the new version cannot read.
When a release changes the database format, a member starting on the new version drops its old log and writes a snapshot of its migrated database in its place, but only if it has already applied every entry in that log.
Otherwise it refuses to start and asks you to run the previous version until every member has applied the whole log.
So let the group settle before stopping it:

1. Take a backup through any member with `kelp backup`.
2. Wait until nobody is submitting or locking, and `kelp cluster` shows every member applied as far as the leader.
3. Stop every member.
4. Replace the binary on every member.
5. Start every member, then check the group:

   ```console
   $ kelp cluster
   ```

   Every member should be listed, with one leader, and each should catch up with the leader's log.

If a member refuses to start because of unapplied entries, start the previous version on every member again, wait as in step 2, and repeat.
The group is unavailable while its members are stopped.
Upgrade edges of the group afterwards, as described above.

## Going back

A data directory that a newer version has started on cannot be opened by an older one.
To go back, stop the server, keep the upgraded data directory aside in case you need it, and restore the backup taken before the upgrade with the older binary:

```console
$ mv /srv/kelp /srv/kelp-0.2.0
$ kelp-server --data /srv/kelp restore /srv/backups/kelp-before-0.2.0
```

Anything submitted after the backup is lost.
The backup keeps the TLS identity, so clients reconnect without trusting the server again.
