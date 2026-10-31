# Webhooks

A webhook sends events to a URL as they happen.

```console
$ kelp webhook set ci https://ci.studio/kelp --event submit --event review --path //game/...
$ kelp webhook list
$ kelp webhook delete ci
```

`set` asks for a signing secret, or reads it with `--secret-stdin`.
`--event` matches an event kind or prefix; `review` matches every review event.
`--path` matches the files an event concerns.
Omitting either matches everything.

## Events

`submit`, `shelve`, `shelf.delete`, `review.open`, `review.vote`, `review.status`, `review.close`, `issue.link`, `lock`, `unlock`, `depot.create`, `stream.create`, `artifact.publish` and `obliterate`.

Commands about accounts, tokens, settings and webhooks produce no events.

## Deliveries

Each event is a `POST` with a JSON body holding `event`, `delivery`, `log_index`, `actor` and `data`, and these headers:

| Header | Value |
|---|---|
| `X-Kelp-Event` | the event kind |
| `X-Kelp-Delivery` | the delivery id, the same on every retry |
| `X-Kelp-Signature` | `sha256=` and the hex HMAC-SHA256 of the body with the secret |

Verify the signature before trusting a delivery, and use the delivery id to ignore duplicates.

A 2xx response is success.
Other responses are retried with exponential backoff starting at 10 seconds, 8 times in all.
Deliveries are queued in the same transaction as the command, so a restart loses none.

```console
$ kelp webhook deliveries ci
$ kelp webhook redeliver 1042
```
