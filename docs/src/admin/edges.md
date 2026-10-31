# Edge Servers

An edge server replicates a commit server's metadata and caches its chunks, for an office far from the commit server.
Clients connect to the edge as to any server.

- Reads, such as sync, history and permission checks, are served locally.
- Writes, such as submits and locks, are forwarded to the commit server with the client's own token, and answered once the edge has replicated them, so a client always reads its own writes.
- Uploaded chunks are written to both, so the rest of the office syncs them from the edge.

## Setting up an edge

On the commit server, create the account the edge replicates with:

```console
$ kelp user add edge-eu --replica
$ kelp token edge-eu > edge-eu.token
```

On the edge machine, serve an empty data directory:

```console
$ kelp-server --data /srv/kelp-edge serve \
    --upstream commit.studio:7474 \
    --upstream-fingerprint <fingerprint> \
    --token-file edge-eu.token
```

An edge needs no `init`.
`--upstream` can list several members of a [commit group](commit-groups.md), separated by commas; the edge uses whichever answers.

The edge checks every replicated entry against the commit server's audit hash.
If they ever differ, it stops replicating and refuses requests rather than serve diverged data.
