# Configuration Files

## server.toml

`<data>/server.toml` holds optional server settings.
The server reads it on start.

### `[storage]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `kind` | `"local"`, `"s3"` or `"tiered"` | `"local"` | where chunks are stored |

### `[storage.s3]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `bucket` | string | required | the bucket name |
| `region` | string | from the environment | the bucket's region |
| `endpoint` | string | AWS | a custom endpoint, for MinIO, R2 and the like |
| `prefix` | string | none | a key prefix, so servers can share a bucket |
| `allow_http` | bool | `false` | allows a plain HTTP endpoint |

Credentials come from the `AWS_*` environment variables.

### `[oidc]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `issuer` | string | required | the provider's issuer URL |
| `client_id` | string | required | Kelp's client id at the provider |
| `client_secret` | string | none | for providers that require one |
| `scopes` | string | `"openid profile email"` | requested scopes |
| `username_claim` | string | `"preferred_username"` | the claim that names a new user |
| `expect_claim` | string | `username_claim` | the claim a first login is matched against [expected names](../admin/sso.md#how-accounts-are-linked); `"sub"` for the subject, `"email"` for a verified address |
| `groups_claim` | string | none | a claim listing group names |
| `auto_create` | bool | `true` | whether a first login creates the user |
| `require` | bool | `false` | whether users other than administrators must use single sign-on |
| `managed_groups` | list of strings | `[]` | groups whose membership the provider controls fully |

See [Single Sign-On](../admin/sso.md).

## cluster.toml

`<data>/cluster.toml` makes a server a member of a [commit group](../admin/commit-groups.md).

| Key | Type | Meaning |
|---|---|---|
| `id` | integer | this member's id, unique in the group |
| `secret` | string | a shared secret that authenticates Raft messages |
| `[members]` | table | every member's id and address |

## Workspace config.toml

`<workspace>/.kelp/config.toml` holds a workspace's settings.

| Key | Type | Meaning |
|---|---|---|
| `name` | string | the workspace name, unique per user |
| `server` | string | the server address |
| `stream` | string | the stream the workspace is bound to |
| `view` | string | the [view](../guide/workspaces.md#the-view) |
| `projected` | bool | whether the workspace is [projected](../guide/projected.md) |
| `[[artifact]]` | array of tables | [artifacts](../guide/artifacts.md#placing-artifacts-in-workspaces) that sync places |

Each `[[artifact]]` has:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | the artifact name |
| `to` | string | required | the workspace directory it is placed in |
| `stream` | string | the workspace's | the stream that publishes it |
| `exact` | bool | `false` | whether it must be built from exactly the synced changelist |
| `tags` | table | `{}` | tags it must have |

## Typemaps

A typemap is a list of `[[rule]]` tables, then optional `[[trigger]]` tables and an optional `[issues]` table.
See [Locks and the Typemap](../guide/typemap.md).

| Rule key | Values | Default | Meaning |
|---|---|---|---|
| `pattern` | glob | required | the stream-relative paths the rule applies to |
| `kind` | `"text"`, `"binary"`, `"auto"` | `"auto"`, sniffed | how the content is treated |
| `lock` | `"exclusive"`, `"none"` | `"none"` | whether editing requires a lock |
| `eol` | `"normalize"`, `"preserve"` | `"preserve"` | whether text line endings are normalized on submit |
| `compress` | bool | `true` | whether chunks are compressed at rest |
| `retain` | integer | all | how many newest revisions keep their content |
| `chunker` | `"plugin:<name>@<version>"` | built-in | the chunker plugin |
| `merge` | `"builtin:3way"`, `"plugin:<name>@<version>"` | `"builtin:3way"` for text | the merge driver |
| `differ` | `"plugin:<name>@<version>"` | none | the plugin that describes changes |

| Trigger key | Meaning |
|---|---|
| `pattern` | the paths whose changes run the trigger |
| `plugin` | the trigger plugin, as `plugin:<name>@<version>` |

| Issues key | Meaning |
|---|---|
| `prefixes` | the issue key prefixes that are recognized; all if omitted |
