# Command Summary

Every command prints its options with `--help`.
This page lists them all.

## kelp

| Command | Description |
|---|---|
| `kelp login` | Logs in to a server, trusting its certificate on first use |
| `kelp totp enable` | Requires a code from an authenticator app when you log in |
| `kelp totp disable` | Stops requiring a code; administrators can do this for any user |
| `kelp logout` | Forgets the token for a server |
| `kelp passwd` | Changes your password, or with `--user`, another user's (administrators only) |
| `kelp user add` | Creates a user |
| `kelp user show` | Shows a user, by current or former name |
| `kelp user list` | Lists every user |
| `kelp user disable` | Stops a user from logging in and revokes their tokens; their history and locks stay |
| `kelp user enable` | Lets a disabled user log in again |
| `kelp user link` | Lets the first single sign-on as `sso_name` log in as `name` and link the two, for a provider user name that differs |
| `kelp user unlink` | Removes a user's single sign-on link |
| `kelp user merge` | Merges user `from` into `into`, or renames `from` if `into` does not exist; history keeps the old name, which becomes an alias |
| `kelp token` | Issues an access token, typically for a service account (administrators only) |
| `kelp group set` | Sets the members of a group; no members deletes it |
| `kelp group list` | Lists groups |
| `kelp protect get` | Prints the current document |
| `kelp protect set` | Replaces the document with the contents of a file |
| `kelp backup` | Writes a backup of the server into a directory on the server's machine (administrators only). Restore it with `kelp-server restore` |
| `kelp fsck` | Checks that every file the server keeps can be read (administrators only) |
| `kelp relocate` | Points the workspace, and your login, at a server's new address after it moved, for example after `kelp-server restore` on another machine |
| `kelp gc` | Deletes content nothing refers to any more (administrators only) |
| `kelp tier` | Moves history no stream currently uses to cold storage (administrators only; needs tiered storage) |
| `kelp obliterate` | Permanently removes files and their history (administrators only). A path ending in `/...` names a directory |
| `kelp audit` | Shows or verifies the audit log (administrators only) |
| `kelp depot create` | Creates a depot and its `main` stream |
| `kelp plugin add` | Uploads a component and registers it (administrators only). Typemaps refer to it as `plugin:<name>@<version>` |
| `kelp plugin list` | Lists registered plugins |
| `kelp plugin remove` | Deletes a plugin version (administrators only) |
| `kelp webhook set` | Creates or replaces a webhook; prompts for the signing secret |
| `kelp webhook list` | Lists webhooks |
| `kelp webhook delete` | Deletes a webhook |
| `kelp webhook deliveries` | Lists recent deliveries, newest first |
| `kelp webhook redeliver` | Sends a delivery again |
| `kelp issue show` | Shows the changes linked to an issue, such as `PROJ-123`, and the streams they have reached |
| `kelp issue link` | Links a submitted changelist to issues, or unlinks it |
| `kelp issue in` | Lists the issues whose changes a stream contains, or a build artifact was built from, optionally only those added since an earlier point |
| `kelp review open` | Asks for a review of a shelf; shelving again adds a version |
| `kelp review list` | Lists reviews, newest first |
| `kelp review show` | Shows a review's files, votes, statuses and requirements |
| `kelp review approve` | Approves the latest version |
| `kelp review reject` | Rejects the latest version, which blocks the submit |
| `kelp review unvote` | Withdraws your vote |
| `kelp review status` | Reports the outcome of an automated check, typically from CI |
| `kelp review close` | Closes a review without submitting |
| `kelp review rules` | Prints the review rules table |
| `kelp review set-rules` | Replaces the review rules table with a file (administrators only) |
| `kelp artifact publish` | Publishes the files below a directory as an artifact built from a changelist |
| `kelp artifact list` | Lists artifacts, newest first |
| `kelp artifact get` | Downloads an artifact into a directory |
| `kelp artifact pin` | Keeps an artifact regardless of retention (administrators only) |
| `kelp artifact delete` | Deletes an artifact (administrators only) |
| `kelp artifact retain` | Keeps only the newest artifacts of a name (administrators only) |
| `kelp typemap get` | Prints the current document |
| `kelp typemap set` | Replaces the document with the contents of a file |
| `kelp stream create` | Creates a stream branching from a parent |
| `kelp stream delete` | Deletes a stream, keeping its history. Administrators delete any stream but a mainline; others delete task streams |
| `kelp stream imports get` | Prints the current document |
| `kelp stream imports set` | Replaces the document with the contents of a file |
| `kelp streams` | Lists streams |
| `kelp cluster` | Shows the commit group as the server sees it (administrators) |
| `kelp init` | Creates a workspace bound to a stream, in `dir` or the current directory |
| `kelp unmount` | Unmounts the projected workspace in the current directory until kelpd mounts it again |
| `kelp sync` | Brings the workspace, or only `paths`, to a changelist or label, or to the head of its stream: `kelp sync`, `kelp sync @120`, `kelp sync Content/Maps @ship-1.0` |
| `kelp label set` | Sets a label, moving it if it exists |
| `kelp label delete` | Deletes a label |
| `kelp label list` | Lists labels |
| `kelp status` | Shows opened files and unopened changes |
| `kelp reconcile` | Opens all unopened changes in the default changelist |
| `kelp add` | Opens new files for add |
| `kelp edit` | Opens files for edit, locking exclusive files |
| `kelp delete` | Opens files for delete and removes them from disk |
| `kelp revert` | Reverts opened files |
| `kelp resolve` | Resolves files changed both here and in the stream |
| `kelp cl new` | Creates a pending changelist |
| `kelp cl list` | Lists pending changelists |
| `kelp cl delete` | Deletes a pending changelist, moving its files to the default changelist |
| `kelp cl move` | Moves opened files to another changelist |
| `kelp cl split` | Lists the hunks of an opened text file, or moves one to another changelist |
| `kelp submit` | Submits a pending changelist |
| `kelp shelve` | Shelves a pending changelist on the server |
| `kelp unshelve` | Opens the files of a shelf in this workspace |
| `kelp delete-shelf` | Deletes a shelf |
| `kelp shelves` | Lists shelves with files you can read, newest first, of one `--stream` or `--user` if given |
| `kelp merge` | Merges changes from another stream (default: the parent) into a new changelist. With `--cl`, picks only that changelist's changes, from the stream it was submitted to unless `--from` names another |
| `kelp undo` | Opens the reverse of a submitted changelist in a new changelist |
| `kelp copy-up` | Copies this workspace's stream into its parent |
| `kelp locks` | Lists locked files in the stream's lock domain |
| `kelp unlock` | Releases locks; `--force` releases anyone's (administrators only) |
| `kelp snapshot save` | Saves the opened files of a changelist |
| `kelp snapshot list` | Lists snapshots |
| `kelp snapshot restore` | Restores a snapshot's files and reopens them |
| `kelp snapshot delete` | Deletes a snapshot |
| `kelp snapshot diff` | Shows the differences between two snapshots, or a snapshot and the disk; `--tool` opens each file in a diff tool |
| `kelp fstat` | Shows the state of files: opened, modified, locked, out of date |
| `kelp diff` | Shows how files differ from the versions synced: opened files and unopened changes, under `paths` if given. Files whose type names a differ plugin are shown as it describes them; `--tool` opens each file in a diff tool instead |
| `kelp clean` | Puts files that are not opened back as synced and deletes files the workspace does not track. The discarded changes are not kept |
| `kelp move` | Moves or renames a file, or a directory of files, keeping their history |
| `kelp annotate` | Prints each line of a text file with the changelist and author that last changed it |
| `kelp print` | Prints a file as of a changelist: `kelp print Source/main.rs@120`. Depot paths such as `//engine/main/a.h` work too |
| `kelp filelog` | Lists the revisions of a file |
| `kelp changes` | Lists recent changelists |
| `kelp describe` | Shows one changelist |

## kelp-server

| Command | Description |
|---|---|
| `kelp-server init` | Creates the data directory and the first administrator |
| `kelp-server backup` | Writes a backup of a stopped server into a directory; backing up into the same directory again only copies new chunks. While the server runs, use `kelp backup` instead |
| `kelp-server restore` | Creates the data directory from a backup, for example on another machine, and checks it |
| `kelp-server fsck` | Checks that every file in use can be read |
| `kelp-server service install` | Writes a unit for this data directory, enables it at boot and starts it. A system unit needs root; `--user` installs one for the current user instead |
| `kelp-server service uninstall` | Stops and disables the service and removes its unit |
| `kelp-server service status` | Shows the service's state and recent log |
| `kelp-server serve` | Serves the protocol until interrupted |
