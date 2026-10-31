# Permissions

The protections table decides who can read and write which paths.
An administrator replaces it as a whole:

```console
$ kelp protect get > protections.txt
$ kelp protect set protections.txt
```

```text
# level  who              path
write    *                //...
read     group:artists    //engine/...
none     group:outsource  //game/main/Secret/...
write    user:bob         //game/main/Secret/bob.txt
```

Each line has a level, a subject and a path.

- The level is `none`, `read` or `write`.
- The subject is `*`, `user:<name>` or `group:<name>`.
- A path ending in `/...` covers a directory; any other path names one file.

For a user and a path, the last matching line wins.
Before an administrator sets a table, it is `write * //...`.
Administrators are not limited by the table.

## What is checked

Read permission filters what the server returns.
Sync, diff and history do not show files a user cannot read, and a stream with no readable files does not appear to exist.

Write permission is checked on submit, lock, shelve and copy-up.
Publishing an artifact requires write permission on `<stream>/.artifacts/<name>`.

Creating depots and development or release streams, and every command marked "administrators only" in its help, require an administrator.
