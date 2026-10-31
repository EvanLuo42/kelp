# Locks and the Typemap

## The typemap

Each depot has a typemap: an ordered list of rules that give files a type.
The type decides whether a file is text or binary, whether it must be locked, and how it is stored and merged.

```toml
[[rule]]
pattern = "**/*.{png,psd,tga,exr,fbx,blend,wav,uasset,umap}"
kind = "binary"
lock = "exclusive"

[[rule]]
pattern = "**/*.{mp4,ogg,zip}"
kind = "binary"
lock = "exclusive"
compress = false

[[rule]]
pattern = "Build/Baked/**"
kind = "binary"
retain = 3

[[rule]]
pattern = "**/*.{rs,cpp,h,hlsl,toml,json}"
kind = "text"
eol = "normalize"
merge = "builtin:3way"
```

Patterns match stream-relative paths case-insensitively.
Rules apply in order, and a later rule overrides only the fields it sets.
A file no rule matches is sniffed as text or binary and is not locked.
The fields are listed in [Configuration Files](../reference/config.md#typemaps).

Only an administrator can change the typemap:

```console
$ kelp typemap get --depot game > typemap.toml
$ kelp typemap set typemap.toml --depot game
```

The server applies the typemap on every submit, so it cannot be bypassed by editing a file in a workspace.

## Exclusive locks

`kelp edit` locks files whose type has `lock = "exclusive"`.
Only the holder can submit them.
Submitting or reverting a file releases its lock.

```console
$ kelp locks
$ kelp unlock Content/Hero.uasset
$ kelp unlock --force Content/Hero.uasset
```

`--force` releases another user's lock and requires an administrator.

A development or task stream shares locks with its parent, so the same asset cannot be edited in `main` and in a child stream at the same time.
Mainline and release streams each have their own locks.

In an ordinary workspace, exclusive files are read-only until you open them with `kelp edit`.
