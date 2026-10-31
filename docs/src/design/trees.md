# Trees

Every stream has, at every changelist, a content-addressed tree of its files.

Directories form a Merkle tree, as in Git: an entry points at a subdirectory by the hash of its tree.
Inside one directory, entries are kept in a *prolly tree*, keyed by the case-folded name.

| Property | From |
|---|---|
| One hash comparison tells whether a directory changed | Merkle directories |
| Creating a stream takes constant time: it points at its parent's root | Merkle directories |
| An import grafts a subtree by its hash; a moved directory keeps its subtree | Merkle directories |
| A directory of tens of thousands of entries is split into bounded nodes; one change rewrites O(log n) nodes | prolly trees |
| The same entries give the same hash, whatever order of edits produced them | prolly trees |
| A diff skips every subtree whose hash is unchanged | both |

## Prolly trees

A prolly tree is a B-tree whose node boundaries are chosen by content, not by insertion order, so it is *history-independent*.

- A node is cut when it reaches the maximum size, never below the minimum, and in between when `hash(level ‖ key)`, scaled, is below `size − min`.
  The cut probability rises with the size.
  Only integer arithmetic is used, so every platform gets identical trees.
- The single node of the top level is the root, and a root with one child collapses into it.
  Building in bulk and editing in any order give the same root.
- An edit re-chunks each level from the first affected node until a new boundary meets an old one, and passes the changed node keys up as point edits.
- Nodes are 2 KiB minimum and 16 KiB maximum.

Property tests check history independence, node size bounds, parent keys and that a single edit rewrites O(height) nodes.

## Paths

Paths keep their original case, with a case-folded key (Unicode simple case folding after NFC normalization) for lookups.
Two paths that differ only in case or normalization cannot coexist, because they would collide on Windows and macOS.
Paths containing `..`, empty segments, Windows reserved names, trailing spaces or dots, or control characters are rejected.
