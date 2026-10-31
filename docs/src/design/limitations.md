# Known Limitations

- **Merge records are trusted.**
  For a merge that changes files, the server checks that the recorded merge point does not exceed the source's head, but not that the result contains every source change; a resolved conflict can legitimately differ from both sides.
- **Content reads are not checked against paths.**
  Fetching chunks and manifests by hash requires a login but not read permission on a path.
  The hashes are only revealed through paths a user can read, but anyone who learns a hash elsewhere can read the content.
- **Coarse timestamps.**
  On file systems with one-second timestamps, such as FAT, a change within the same second that keeps the size can go unnoticed by `status`.
- **Request size.**
  A single request is limited to 16 MiB.
  A submit of hundreds of thousands of files must be split into several submits.
- **Global administrators.**
  There are no per-depot administrators or partial administrative roles.
- **macOS.**
  Projected workspaces are not available on macOS.
