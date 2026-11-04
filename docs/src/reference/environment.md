# Environment Variables

| Variable | Used by | Meaning |
|---|---|---|
| `KELP_HOME` | `kelp`, `kelpd` | the directory for credentials, the content cache and kelpd's socket; defaults to `kelp/` in the platform's config directory |
| `KELP_NO_DAEMON` | `kelp` | when set to a true value, runs workspace commands in the `kelp` process instead of kelpd |
| `KELP_DIFF_TOOL` | `kelp` | the diff tool `diff` and `snapshot diff` open files in when `--tool` is not given, such as `meld {old} {new}`; see [Workspaces](../guide/workspaces.md#diff-tools) |
| `KELP_LOG` | `kelp`, `kelpd`, `kelp-server` | the log filter, such as `debug` or `info,kelp_server=trace`; overrides `-v` |
| `RUST_LOG` | `kelp`, `kelpd`, `kelp-server` | the log filter when `KELP_LOG` is not set |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`, `AWS_REGION` | `kelp-server` | credentials and region for S3 storage |
| `USER`, `USERNAME` | `kelp`, `kelp-server` | the default user name for `login` and `init` |
