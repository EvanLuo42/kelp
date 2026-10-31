# Plugins

Plugins add format-specific behavior.
They are WebAssembly components described by `crates/kelp-plugin/wit/kelp.wit`, and implement one or more worlds:

| World | Does | Runs on | Typemap field |
|---|---|---|---|
| `chunker` | chooses chunk boundaries | client, on submit and shelve | `chunker = "plugin:x@1"` |
| `merger` | merges three versions of a file | client, on resolve | `merge = "plugin:x@1"` |
| `differ` | describes a change for people | client, on `review show` | `differ = "plugin:x@1"` |
| `trigger` | accepts or rejects a submit | server | `[[trigger]]` |

A chunker affects only deduplication, never correctness.
If it fails, or returns a chunk larger than 4 MiB, Kelp falls back to its built-in chunker.

A trigger can read the submitted files.
If it rejects the submit or fails, the submit is rejected.

```toml
[[trigger]]
pattern = "Content/**"
plugin = "plugin:asset-check@1"
```

## Registering plugins

```console
$ kelp plugin add asset-check 1 asset_check.wasm
$ kelp plugin list
$ kelp plugin remove asset-check 1
```

Registering requires an administrator.
A version can not be replaced, only removed and registered again, so `plugin:<name>@<version>` refers to the same bytes on every machine.
Clients download plugins when they first need them.

## Sandbox

Plugins get no file system, network, clock or randomness: only their arguments, and for triggers the submitted files.
Each call runs in a fresh instance with limits on fuel and memory.
A component that imports anything else is refused when it is registered.

`crates/kelp-plugin/guests` has example plugins written with `wit-bindgen`.
