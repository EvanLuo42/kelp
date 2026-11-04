# Known Limitations

- **Merges are checked for completeness, not correctness.**
  The server refuses a merge that leaves out a change of its source, but it trusts the content of every file the merge resolves, since a resolved conflict can legitimately differ from both sides.
- **Uploads reveal whether content exists.**
  Asking which chunks the server lacks, as every submit does, tells a user whether a chunk is stored, though not what it belongs to or what it holds.
- **Content is readable through its history.**
  A user who may read a path may read every version that path ever had, including versions shelved there, and the versions a stream started with when it was created.
- **Request size.**
  A single message, such as a submit, is limited to 1 GiB, which is several million files.
- **macOS.**
  Projected workspaces are not available on macOS.
