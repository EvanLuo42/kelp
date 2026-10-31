# CLAUDE.md

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/): `type(scope): summary`, for example `fix(meta): keep aliases when merging users`.

- Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `build`, `ci`, `chore`.
- Scope is the crate name without `kelp-` (`meta`, `server`, `cli`, ...) or `docs`/`ci`.
- Keep it short: one subject line under ~60 characters, imperative, no trailing period.
  Add a body only when the reason is not obvious from the diff.

## Markdown

Write one sentence per line and do not hard-wrap at a column.
Separate paragraphs with a blank line; sentences within a paragraph are separated by a single newline.
This keeps VCS diffs small.

## Things that are not obvious from the code

- **S3 tests skip silently.**
  Tests using `kelp_testkit::s3::config()` pass without doing anything unless `KELP_TEST_S3_ENDPOINT` and `KELP_TEST_S3_BUCKET` are set.
  To really run them, start moto (`pipx install "moto[server]"`, `moto_server -p 15000`), create the bucket with `curl -X PUT http://127.0.0.1:15000/kelp-test`, and export those two variables plus any `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`.
  See `.github/workflows/ci.yml`.
- **Plugin tests build WebAssembly on first use.**
  `kelp_testkit::wasm` runs `cargo build` on `crates/kelp-plugin/guests` (a separate workspace, not part of the main one) into `target/wasm-guests`.
  It needs `rustup target add wasm32-unknown-unknown`, and the first run is slow.
- **Platform-specific VFS.**
  `kelp-vfs` uses FUSE on Linux and ProjFS on Windows; `crates/kelp-vfs/tests/fuse.rs` only compiles on Linux.
- **Use `--all-features`.**
  Many tests sit behind testkit features (`s3`, `meta`, `wasm`, `net`); `cargo test --workspace --all-features` matches CI.
- **Manual CLI runs.**
  Set `KELP_HOME` to a scratch directory so the CLI does not touch the real config, and `KELP_NO_DAEMON=1` (or `--no-daemon`) to run workspace commands in-process instead of through `kelpd`.
- **Book.**
  `mdbook build docs` needs `mdbook-mermaid` installed as well.
- **Releases.**
  Bump the workspace version *and* every `kelp-*` entry in `[workspace.dependencies]` in `Cargo.toml`, and rename `Unreleased` in `CHANGELOG.md`; the release workflow fails if the tag and version differ.
