# Single Sign-On

Kelp supports OpenID Connect providers such as Google Workspace, Entra ID, Okta and Keycloak.
Configure the provider in `server.toml`:

```toml
[oidc]
issuer = "https://login.studio.example"
client_id = "kelp"
groups_claim = "groups"
```

The full list of fields is in [Configuration Files](../reference/config.md#oidc).
Register Kelp with the provider as a client that uses the device authorization grant.

## Logging in

```console
$ kelp login kelp.studio:7474 --sso
```

The CLI prints a URL and a code.
Approve the login in a browser; the CLI then receives a Kelp token.
The server talks to the provider, so clients and edges need no access to it.

## How accounts are linked

A provider account is linked to a Kelp user by its issuer and subject, which never change.
The provider's user name, from `username_claim`, is used only on the first login:

1. If an administrator linked the name ahead of time, the login links to that user.
   The name is compared with the `expect_claim` claim, which is the user name unless configured otherwise.
2. Otherwise, if no Kelp user has the name and `auto_create` is on, a new user is created and linked.
3. Otherwise the login is refused.
   A provider user name can be changed or reassigned, so matching it to an existing user would let anyone take over that user.

After linking, the user logs in only through single sign-on.
Their password, two-factor secret and older tokens are removed.

## Moving existing users to single sign-on

Users who already have Kelp accounts link them in one of two ways.

The user links their own account, logged in with their password:

```console
$ kelp login kelp.studio:7474 --sso --link
```

Or an administrator links ahead of time, which also handles a provider name that differs from the Kelp name:

```console
$ kelp user link evan --sso evan.luo
$ kelp user unlink evan
```

Whoever first signs on under an expected name becomes that Kelp user.
With many providers, users can change their own user name, or names are handed to new people, so someone could take the name an administrator expects for another user.
Match expected names against a claim the provider controls instead, such as a verified email address:

```toml
[oidc]
expect_claim = "email"
```

```console
$ kelp user link evan --sso evan.luo@studio.example
```

With `expect_claim = "email"`, an address counts only if the ID token marks it verified (`email_verified`).
`expect_claim = "sub"` matches the provider's subject, which never changes but is often an opaque id.
Expected names that were set up before changing `expect_claim` keep their old values, so set them again.

If a user already has a second account created by single sign-on, merge the old one into it; see [Renaming and merging](accounts.md#renaming-and-merging).

## Requiring single sign-on

With `require = true`, users other than administrators can no longer log in with a password.
Administrators keep their passwords for when the provider is down.
Service accounts are unaffected.

## Group sync

Groups listed in `groups_claim` are joined on login when a Kelp group of the same name exists.
Groups listed in `managed_groups` are fully controlled by the provider: a user missing from the claim is removed from them.
Membership in other groups is only ever added.
