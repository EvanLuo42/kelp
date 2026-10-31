# Monitoring

`serve --metrics` serves plain HTTP on a separate address:

```console
$ kelp-server --data /srv/kelp serve --metrics 127.0.0.1:9184
```

`GET /healthz` answers `200 ok` while the server can serve.
It answers `503` with the reason when an edge has stopped replicating or a commit group member sees no leader.
Use it for load balancer and service manager health checks.

`GET /metrics` answers in the Prometheus text format:

| Metric | Type | Meaning |
|---|---|---|
| `kelp_up` | gauge | 1 while the server can serve |
| `kelp_log_index` | gauge | index of the last command in the log |
| `kelp_streams` | gauge | number of streams |
| `kelp_users` | gauge | number of users |
| `kelp_requests_total` | counter | requests served |
| `kelp_request_errors_total` | counter | requests that failed |
| `kelp_edge_applied_index` | gauge | on an edge, the last replicated entry |
| `kelp_cluster_leader` | gauge | in a commit group, 1 on the leader |
| `kelp_cluster_applied_index` | gauge | in a commit group, the last applied Raft entry |

The address serves no authentication; bind it to localhost or a private network.

## Logs

The server logs to standard error, which a systemd service sends to the journal (`journalctl -u kelp-server`).
At the default `info` level it logs startup, logins, every change and every failed request; logins that fail and refused permissions are warnings, and internal errors are errors.
`KELP_LOG=debug` adds every connection and every read, with its user and duration in milliseconds.
`KELP_LOG` takes a filter such as `info,kelp_server::handler=debug`, and `RUST_LOG` is read when it is not set.
Server logs never contain passwords, tokens or request contents.

kelpd logs to `logs/kelpd.log.<date>` in the Kelp home directory and keeps a week of files.
The `kelp` command logs warnings to standard error; `-v`, `-vv` and `-vvv` log more.
