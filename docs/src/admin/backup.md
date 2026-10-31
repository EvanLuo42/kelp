# Backup and Recovery

A backup is itself a data directory.
Backing up into the same directory again copies only new chunks and replaces the metadata last, so an interrupted backup leaves the previous one usable.

While the server runs, an administrator writes a backup on the server's machine:

```console
$ kelp backup /srv/backups/kelp
```

While it is stopped:

```console
$ kelp-server --data /srv/kelp backup /srv/backups/kelp
```

With `--metadata-only`, chunks in object storage are left where they are; rely on the bucket's own versioning for them.

## Checking

```console
$ kelp fsck
$ kelp fsck --deep
$ kelp-server --data /srv/kelp fsck
```

`fsck` checks that every file in use can be read.
`--deep` also reads and hashes every chunk.

## Restoring and moving

`restore` creates a data directory from a backup and checks it:

```console
$ kelp-server --data /srv/kelp restore /srv/backups/kelp
```

The backup includes the TLS identity, so the fingerprint stays the same and clients only need the new address.
In each workspace:

```console
$ kelp relocate kelp.studio:7474
```

`--new-identity` generates a new certificate instead; clients must then trust the server again.

To move a server from one machine to another, back it up, copy the backup, restore it, and relocate the workspaces.
