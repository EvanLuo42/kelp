# Accounts

Kelp has its own accounts.
Every user has a name, which history and permissions refer to, and a way to log in: a password, single sign-on, or tokens only.

## Users

```console
$ kelp user add alice
$ kelp user add bob --admin
$ kelp user list
$ kelp user show alice
```

Managing users requires an administrator.
Administrators are not limited by [permissions](permissions.md) or [review gates](../guide/reviews.md#gate-rules).

When someone leaves, disable their account.
Disabling revokes their tokens at once and keeps their history and locks:

```console
$ kelp user disable alice
$ kelp user enable alice
```

## Passwords

```console
$ kelp passwd
$ kelp passwd --user alice
```

Users change their own password.
An administrator can set anyone's with `--user`.
Passwords have at least 8 characters and are stored as argon2id hashes.

## Two-factor login

```console
$ kelp totp enable
$ kelp totp disable
$ kelp totp disable --user alice
```

`enable` prints an `otpauth://` URI for an authenticator app and asks for one code to confirm.
From then on, a password login also asks for a code.
Turning it off requires a current code, except for an administrator acting on another user.
To move to a new authenticator app, turn two-factor login off and enable it again; `enable` refuses while it is on.

## Groups

```console
$ kelp group set artists alice carol
$ kelp group list
$ kelp group set artists
```

Setting a group with no members deletes it.
Groups are used by [permissions](permissions.md) and review rules as `group:<name>`.

## Service accounts and tokens

Build machines and bots use service accounts, which have no password and log in with a token:

```console
$ kelp user add ci --service
$ kelp token ci --days 90 > ci.token
```

On the build machine, log in with the token once:

```console
$ kelp login kelp.studio:7474 --token-stdin < ci.token
```

Tokens can also be issued for ordinary users.
A token stops working when it expires, or when its user is disabled.

`kelp logout` forgets the token for a server on this machine.

## Renaming and merging

```console
$ kelp user merge evan evan.luo
```

`merge` moves everything `evan` owns, such as group memberships, tokens, locks, shelves and reviews, to `evan.luo`, or renames `evan` if `evan.luo` does not exist.
Protections and review rules that name `user:evan` are updated.
History keeps the old name, because the audit log cannot be rewritten.
The old name becomes an alias: it still logs in and can not be taken by a new user.
`kelp user show` lists a user's aliases.
