# Reviews

A review records who approved a shelf and what automated checks reported, so that a submit can be gated on them.
Discussion belongs in a separate code review tool, which can drive Kelp through the CLI or the protocol.

```console
$ kelp shelve -c L1
Shelved as 42.
$ kelp review open 42 --reviewer bob
$ kelp review show 42
```

Reviewers vote on the latest version:

```console
$ kelp review approve 42
$ kelp review reject 42
$ kelp review unvote 42
```

CI reports checks:

```console
$ kelp review status 42 build success --url https://ci.studio/run/77
```

Then the author submits as usual:

```console
$ kelp submit -c L1
```

Shelving the changelist again adds a version to the review.
Votes and statuses belong to a version, so a change after approval needs a new approval.
A submit of a reviewed changelist must match the latest version exactly; otherwise shelve again.
After the submit, the review is marked submitted and the shelf is deleted.

`kelp review list` lists open reviews; `--all` includes closed ones.
`kelp review close` closes a review without submitting.

## Gate rules

An administrator sets the rules that a submit must satisfy:

```text
# approve <count> <who> <path>
# status  <check> <path> [by <who>]
# issue   <path>
approve 1 group:engine-leads //engine/...
approve 2 *                  //game/main/Content/...
status  build                //game/...  by group:ci
issue                        //game/rel-1.0/...
```

```console
$ kelp review set-rules rules.txt
$ kelp review rules
```

A submit must satisfy every rule whose path matches one of its files.

- `approve` requires that many approvals from users matching `who` on the latest version.
  The author's own vote does not count.
- `status` requires the latest report of the check on the latest version to be `success`.
  With `by`, only reports from those users count.
- `issue` requires the changelist to be linked to an [issue](issues.md).

Any rejection on the latest version blocks the submit.
Administrators are not gated.
Because copy-up has no review, only an administrator can copy up into a stream that rules cover.
