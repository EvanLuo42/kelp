# The Metadata Log

Every write is a command appended to a log and applied by a deterministic state machine to redb.
A command carries everything the state machine needs, including timestamps, so replaying the log on any replica gives the same database.
Changelist numbers are assigned when a command is applied, so replicas that apply the same commands in the same order agree on them.

The state machine records how far it has applied the log in the same redb transaction as each command's changes, so a crash never applies a command twice.
A command that is rejected, for example by a conflict, is rejected the same way everywhere; only the marker advances.

## Submit

1. The client finds which chunks the server lacks, uploads them, then uploads the manifests.
   The server checks that every chunk exists and that the manifest hashes to its content id.
2. The client sends the submit: for each file, its action, new content and the content it started from.
3. The server checks permissions, locks, review gates and triggers, and that each file's current content equals the content the client started from.
4. The server builds the new tree and applies the command.

Checking content rather than revision numbers is what lets a stream share its tree with its parent: revision numbers mean nothing across streams, and equal content means no change can be lost.

## Audit chain

Each applied command is recorded with its author in a hash chain, `hash = H(previous, index, actor, command)`.
Edges recompute the chain as they replicate and stop if it differs.
`kelp audit --verify` recomputes it on the server.

## Commit groups

Members agree on proposals with Raft (openraft) and keep its log in `raft.redb`.
The leader applies writes; followers forward writes to it and answer once they have applied the leader's commit index.
The founder may start with history that is in no log entry, from its days as a single server, so before it adds the first member it takes a snapshot and new members start from that.
A member with a fresh disk does not vote until it has caught up, so it cannot elect itself from a stale view of the group.

## Format versions

The database records its format version.
A newer server migrates older formats on start.
An older server refuses a newer format.
