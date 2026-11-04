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

- The level is `none`, `read`, `write` or `admin`.
- The subject is `*`, `user:<name>` or `group:<name>`.
- A path ending in `/...` covers a directory; any other path names one file.

For a user and a path, the last matching line wins.
Before an administrator sets a table, it is `write * //...`.
Administrators are not limited by the table.

## Depot administrators

The `admin` level includes `write` and makes the user an administrator of what the line covers:

```text
write  *              //...
admin  group:leads    //game/...
```

The leads may then, within `//game`:

- create and delete development and release streams;
- set the typemap, and the imports of a stream, from streams they can see;
- obliterate files;
- release anyone's locks with `kelp unlock --force`;
- pin, delete and set the retention of artifacts;
- delete others' shelves, labels and reviews, and change others' issue links.

An `admin` line on `//name/...` also lets its users create the depot `name`.
Powers over a depot or stream need a line that covers all of it; obliterating needs one that covers what is obliterated.

## Roles

A `role` line grants part of a server administrator's powers everywhere:

```text
role  accounts      group:it
role  operations    user:ops
role  audit         group:security
role  integrations  user:ci-admin
```

| Role | May |
|---|---|
| `accounts` | create users, set others' passwords, profiles and two-factor login, disable, merge and link users, issue tokens and set groups |
| `operations` | run `gc`, `tier`, `fsck` and `backup`, and see `kelp cluster` |
| `audit` | read and verify the audit log, and read the protections table |
| `integrations` | manage webhooks and plugins |

The `accounts` role does not reach server administrators or replicas: only a server administrator creates them, or changes their password, tokens or links.
Groups it sets carry whatever the table grants them, so give the role only to people trusted with every group.
Setting the protections table and the review rules always takes a server administrator.

## What is checked

Read permission filters what the server returns.
Sync, diff and history do not show files a user cannot read, and a stream with no readable files does not appear to exist.

Write permission is checked on submit, lock, shelve and copy-up.
Content is also checked when it is fetched by its hash: a user receives a file's content only if it appears at a path they can read, as a revision, a shelved file or a file the stream started with; in an artifact of a stream they can see; or in a plugin.
Clients older than this check can still fetch content as users who may read every path.

Publishing an artifact requires write permission on `<stream>/.artifacts/<name>`.

Creating depots and development or release streams, and every command marked "administrators only" in its help, require an administrator, a depot administrator or a role, as described above.
