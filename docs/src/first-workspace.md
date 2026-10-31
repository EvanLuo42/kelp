# Your First Workspace

This chapter starts a server, creates a depot, and submits a file.
Even a single user runs a server, which can listen on localhost only; accounts and history then work the same way as on a team server.

## Starting a server

Create a data directory and the first administrator.
The administrator name defaults to your operating system user name.

```console
$ kelp-server --data ./kelp-data init
Password for evan:
Repeat:
Initialised ./kelp-data with administrator evan.
Server fingerprint: 3f9a…
```

Then serve it:

```console
$ kelp-server --data ./kelp-data serve --listen 127.0.0.1:7474
```

The server uses a self-signed certificate.
Its fingerprint identifies the server to clients.

## Logging in

In another terminal:

```console
$ kelp login 127.0.0.1:7474
```

The first connection to a server prints its fingerprint and trusts it, as SSH does for host keys; compare it with the one `init` printed.
Later connections fail if the fingerprint changes.

## Creating a depot

A *depot* is a top-level namespace such as `//game`.
Creating one also creates its mainline stream, `//game/main`.

```console
$ kelp depot create game
```

## Creating a workspace

A *workspace* is a directory bound to one stream.

```console
$ mkdir ws && cd ws
$ kelp init //game/main
$ kelp sync
```

## Submitting a change

```console
$ echo 'fn main() {}' > main.rs
$ kelp add main.rs
$ kelp submit -m "Add main"
Submitted changelist 1.
```

`kelp status` shows what is opened and what changed on disk without being opened.
`kelp reconcile` opens every such change in the default changelist, which is convenient after editing outside of Kelp.

```console
$ kelp status
$ kelp reconcile
```

The next chapters describe each of these steps in detail.
