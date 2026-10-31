# Running a Server

`kelp-server` keeps everything in one data directory: the metadata database, chunks, the TLS identity and optional settings.
`--data` names it and defaults to `./kelp-data`.

## Initializing

```console
$ kelp-server --data /srv/kelp init --admin evan
```

`init` creates the directory, a self-signed certificate and the first administrator.
`--password-stdin` reads the password from standard input, for scripts.

## Serving

```console
$ kelp-server --data /srv/kelp serve --listen [::]:7474
```

The server speaks QUIC, so the port must be open for UDP.
It prints its fingerprint on start; users compare it with what `kelp login` shows.

## Running as a service

On Linux, `service install` writes a systemd unit for the data directory, enables it at boot and starts it:

```console
$ sudo kelp-server --data /srv/kelp service install --run-as kelp
$ kelp-server --data /srv/kelp service status
$ sudo kelp-server --data /srv/kelp service uninstall
```

`--run-as` names the account the service runs as; it must own the data directory.
`--user` installs a unit for the current user instead, which starts at login, or at boot after `loginctl enable-linger $USER`.
`--print` prints the unit without installing it.
`serve` options such as `--listen` and the edge options are accepted too.

## Settings

Optional settings live in `<data>/server.toml`; see [Configuration Files](../reference/config.md#servertoml).
The server reads the file when it starts, so restart it after a change.
Everything that `kelp` commands change, such as users, protections and typemaps, takes effect immediately.

## Data directory layout

```text
<data>/meta.redb      metadata database and command log
<data>/chunks/        chunks, when stored locally
<data>/tls/           certificate and private key
<data>/server.toml    settings
<data>/cluster.toml   commit group membership
<data>/raft.redb      a commit group member's Raft log
```

Keep `tls/key.der` private: anyone with it can impersonate the server.

## Upgrading

Install the new version and restart the server.
A newer server migrates the data directory on start; an older one refuses a directory a newer version has written.
See [Upgrading](upgrading.md) for backups, edges, commit groups and going back.
