# Patched fjall

This is a vendored copy of `fjall 3.0.1` with a single modification to
`src/locked_file.rs` that replaces `flock()`-based file locking with
`fcntl(F_SETLK)` POSIX record locks on Android targets.

## Why

Rust's `File::try_lock()` uses `flock()` under the hood, which returns
`ENOTSUP` on Android's Bionic libc. `fcntl`-based locking is universally
supported across all Android API levels.

## What changed

Only `src/locked_file.rs` and `Cargo.toml` (added `libc` dep for Android,
pinned `lsm-tree = "=3.0.1"`). All other files are unmodified from the
upstream crate.

## Maintenance

**When upgrading fjall**, copy the new version from the cargo registry into
this directory and re-apply the `locked_file.rs` patch. The patch is
self-contained to that one file.

```
# After updating fjall version in workspace Cargo.toml:
cp -r ~/.cargo/registry/src/*/fjall-<NEW_VERSION>/* patches/fjall/
# Then re-apply the locked_file.rs changes and update Cargo.toml
```
