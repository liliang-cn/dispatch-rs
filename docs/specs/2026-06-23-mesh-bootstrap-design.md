# Design: SSH passwordless mesh bootstrap

Date: 2026-06-23
Status: Approved (pending implementation plan)

## Problem

dispatch-rs runs commands and transfers files across a fleet over SSH, but it
assumes the SSH trust between the caller and each host already exists. A common
prerequisite — and a recurring operational pain — is establishing **passwordless
SSH trust among the hosts themselves**: e.g. after re-imaging a DRBD cluster,
`root@nodeA -> root@nodeB` no longer works because the key mesh was lost, which
breaks any node-to-node operation (DRBD/drbd-reactor failover, sync).

Today this is done by hand: generate/collect each host's public key and append
them to every host's `authorized_keys`. dispatch-rs already has the primitives
(`exec`, `read`, `write`, parallel fan-out, sudo wrapping) to do this; it just
lacks a first-class operation for it.

## Goal

Add a reusable, idempotent operation that bootstraps a full passwordless SSH
mesh for a target user across a set of hosts, built on the existing dispatch
primitives and consistent with the existing builder API.

Non-goals (first version): removing keys, key rotation, non-ed25519 key types,
CA/certificate-based trust.

## Key concept: transport user vs. mesh user

These are independent:

- **Transport user** — how dispatch connects to each host (the `Config.user` /
  `~/.ssh/config`, optionally with `sudo`). Example: `liliang` + passwordless
  sudo.
- **Mesh user** — whom the trust is being established *for*. Example: `root`.

So dispatch may connect as `liliang` (sudo) and build the `root <-> root` mesh.
The mesh user defaults to `root`.

## API

A new builder on `Dispatch`, matching the `exec` / `copy` / `write` style:

```rust
let result = dispatch
    .mesh(["orange1", "orange2", "orange3"])
    .user("root")            // mesh target user; default "root"
    .also_known_hosts(true)  // pre-populate each host's known_hosts; default false
    .parallel(8)             // inherits Config.parallel by default
    .run()
    .await?;
```

`mesh(patterns)` resolves patterns through the same `Inventory` as `exec`.

### Builder options

| Method | Default | Meaning |
|---|---|---|
| `.user(name)` | `"root"` | mesh target user whose `~/.ssh/authorized_keys` is populated |
| `.also_known_hosts(bool)` | `false` | also scan and store peer host keys into the mesh user's `known_hosts` |
| `.parallel(n)` | `Config.parallel` | per-phase concurrency |
| `.key_type(&str)` | `"ed25519"` | generated key type (only `ed25519` supported in v1) |

All remote commands honor the client's `sudo` setting (so a non-root transport
user with passwordless sudo can write `/root/.ssh`).

## Execution model

`run()` proceeds in phases; within each phase all hosts run in parallel
(barrier between phases — phase 2 needs every host's key from phase 1).

**Phase 1 — ensure keypair + collect public keys.** On each host, resolve the
mesh user's home via `getent passwd <user>` (fallback `/root` for root,
`/home/<user>` otherwise). Then:
```sh
install -d -m700 -o <user> -g <user> <home>/.ssh
test -f <home>/.ssh/id_ed25519 || ssh-keygen -t ed25519 -N '' -f <home>/.ssh/id_ed25519 -C '<user>@<host>'
cat <home>/.ssh/id_ed25519.pub
```
Collect `{host -> pubkey}`. A host whose key step fails is recorded as an error
and excluded from the mesh (its pubkey is not distributed, and it does not
receive others' keys), but does not abort the whole run.

**Phase 2 — distribute authorized_keys (idempotent).** On each successful host,
ensure every collected pubkey is present in `<home>/.ssh/authorized_keys`,
appending only the lines that are missing (match on the full key line). Then fix
ownership/permissions (`.ssh` 700, `authorized_keys` 600, owned by the mesh
user). Re-running adds nothing.

**Phase 3 (optional) — known_hosts.** When `also_known_hosts`, on each host run
`ssh-keyscan -t ed25519 <peer-host>...` for the other hosts and merge the
results into `<home>/.ssh/known_hosts` (deduped). This lets subsequent
node-to-node connections succeed under `StrictHostKeyChecking=yes`.

## Result type

```rust
pub struct MeshResult {
    pub hosts: BTreeMap<String, MeshHostResult>,
}
pub struct MeshHostResult {
    pub public_key: Option<String>,  // collected pubkey, None on phase-1 failure
    pub keys_added: usize,           // new authorized_keys lines written this run
    pub known_hosts_added: usize,    // new known_hosts lines (0 unless also_known_hosts)
    pub error: Option<String>,       // first error for this host, if any
}
impl MeshResult {
    pub fn all_success(&self) -> bool;
    pub fn failed_hosts(&self) -> Vec<String>;
}
```

`run()` returns `Ok(MeshResult)` whenever the operation could be attempted;
per-host failures are reported in `MeshResult` (mirroring `ExecResult`). It
returns `Err` only for setup-level failures (e.g. pattern resolution).

## Idempotency & safety

- Existing keypairs are reused, never overwritten.
- `authorized_keys` is append-only with dedup; pre-existing unrelated keys are
  preserved.
- No key is ever removed.
- Permissions/ownership are corrected each run (cheap, safe).

## Edge cases

- **Transport user cannot sudo to mesh user** → phase-1 commands fail on that
  host; recorded as a per-host error.
- **Host unreachable** → per-host error, others still complete.
- **Mesh user missing on a host** → `getent` yields nothing; recorded as error
  rather than silently writing to a wrong home.
- **Partial mesh** → if some hosts fail phase 1, the mesh is built among the
  successful ones; `failed_hosts()` lists the rest.

## Why in dispatch-rs (not the caller)

"Establish SSH trust across a fleet" is a generic batch-SSH operation, reusable
beyond DRBD-HA, and is most naturally expressed on top of dispatch's existing
multi-host exec/read/write primitives. Keeping it here avoids each consumer
re-implementing the same key-distribution dance.

## Module layout

New `src/mesh.rs` exposing `MeshBuilder`, `MeshResult`, `MeshHostResult`;
`Dispatch::mesh(patterns)` constructor in `lib.rs`; re-exports from the crate
root. Implementation composes `exec`/`read`/`write` rather than opening raw
sessions.
