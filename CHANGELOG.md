# Changelog

## 0.3.0

- **Host-key policy** — `Config::host_key_checking` (`Strict` / `AcceptNew` /
  `AcceptAny`, mapping to ssh's `StrictHostKeyChecking`) and
  `Config::known_hosts_file` (e.g. `/dev/null`). Lets managed clusters that
  re-image nodes accept changed keys without failing.

## 0.2.0

- **`write`** — write in-memory content to a remote file, **creating parent
  directories first** (never fails with "No such file or directory"), with
  optional mode.
- **`read`** — fetch a remote file's bytes (binary-safe base64 round-trip).
- **`sudo`** support — run remote commands and writes through `sudo -n` for
  non-root users with passwordless sudo (`Config::sudo`).
- **Per-connection options** — `Config::{user, port, identity, connect_timeout}`.
- **`exec(...).stream(cb)`** — stream stdout/stderr live as chunks arrive.
- **`update`** now reuses a single ssh connection per host for its
  checksum/backup/chmod steps, and reports progress via `update(...).progress(cb)`.
- `copy` now creates the destination's parent directory before `scp`.

## 0.1.0

- Initial release: `exec`, `copy`, `fetch`, `update` across many hosts in
  parallel, resolving ssh-config aliases, wildcards, and `~/.dispatch/config.toml`
  groups.
