# Commit Groups

A commit group is three or five servers that agree on the metadata log with Raft and share one object storage bucket.
It keeps working while a minority of members is down.

## Setting up a group

Start from an existing server, or `init` a new one.
Every member uses the same TLS identity, so copy it to the others and leave their data directories otherwise empty:

```console
$ scp -r /srv/kelp/tls kelp-2:/srv/kelp/tls
```

Every member needs the same `server.toml`, with `kind = "s3"` and the same bucket, and its own `cluster.toml`:

```toml
id = 1
secret = "a long random string, the same on every member"

[members]
1 = "kelp-1.studio:7474"
2 = "kelp-2.studio:7474"
3 = "kelp-3.studio:7474"
```

`id` differs per member.
The member with the lowest id founds the group with the data it has, and the others join it.
Start every member as usual, for example with `service install`.

```console
$ kelp cluster
```

`kelp cluster` shows the members, the leader and how far each has applied the log.

## How it behaves

Clients can connect to any member.
The leader applies writes; other members forward writes to it and serve reads once they have applied the write, so a client always reads its own writes.
When the leader fails, the group elects a new one and forwarded writes are retried.

To add or remove a member, change `[members]` in every `cluster.toml` and restart.
A new member must start with an empty data directory and the shared TLS identity.

A member whose disk was replaced starts empty and does not vote until it has caught up with the leader.

Webhooks are delivered by the leader.
Garbage collection runs on the leader and waits an hour after a leader change.
