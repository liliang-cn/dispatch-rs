# SSH Mesh Bootstrap — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `Dispatch::mesh(...)` — an idempotent operation that establishes passwordless SSH trust for a target user (default `root`) across a set of hosts, built on the existing `exec` primitive.

**Architecture:** A new `MeshBuilder` (matching the `ExecBuilder`/transfer builder style) runs three phases over the resolved hosts using `Dispatch::exec`: (1) ensure each host has an ed25519 keypair for the mesh user and collect its public key; (2) feed the combined key set to every host over stdin and append only the missing lines to `authorized_keys`; (3) optionally `ssh-keyscan` peers into `known_hosts`. All remote commands honor the client's `sudo` setting, so a non-root transport user with passwordless sudo can write `/root/.ssh`. The shell snippets and output parsing are pure functions (unit-tested); the SSH orchestration is verified against a real cluster, matching this crate's existing test strategy (no SSH mock harness).

**Tech Stack:** Rust, tokio, the crate's own `exec` API, `getent`/`ssh-keygen`/`ssh-keyscan` on the remote hosts.

**Spec:** `docs/specs/2026-06-23-mesh-bootstrap-design.md`

---

## File Structure

- **Create `src/mesh.rs`** — `MeshBuilder`, `MeshResult`, `MeshHostResult`, and the pure helpers (`keygen_script`, `distribute_script`, `keyscan_script`, `parse_tagged_count`). Single responsibility: bootstrap SSH trust across hosts.
- **Modify `src/lib.rs`** — add `mod mesh;`, re-export the new public types, add the `Dispatch::mesh(...)` constructor.
- **Modify `README.md`** — add a "Mesh bootstrap" usage example.
- **Modify `CHANGELOG.md`** — add an entry under a new unreleased/0.4.0 section.

---

## Task 1: Pure shell-script + parse helpers (TDD)

**Files:**
- Create: `src/mesh.rs`
- Modify: `src/lib.rs:37-42` (add `mod mesh;`)

- [ ] **Step 1: Add the module declaration so the new file compiles**

In `src/lib.rs`, the `mod` block (currently lines 37-42) becomes:

```rust
mod config;
mod conn;
mod error;
mod exec;
mod inventory;
mod mesh;
mod transfer;
```

- [ ] **Step 2: Write the failing tests**

Create `src/mesh.rs` with only the helpers under test plus a test module:

```rust
//! SSH passwordless mesh bootstrap: establish `user <-> user` trust across hosts.

use crate::exec::shell_quote;

/// Shell script (phase 1): ensure the mesh user has an ed25519 keypair, then
/// print its public key on stdout. Honors sudo via the caller's Conn wrapping.
fn keygen_script(user: &str) -> String {
    let u = shell_quote(user);
    format!(
        r#"set -u
u={u}
home=$(getent passwd "$u" 2>/dev/null | cut -d: -f6 || true)
if [ -z "$home" ]; then echo "mesh: user $u not found" >&2; exit 3; fi
install -d -m 700 "$home/.ssh"
if [ ! -f "$home/.ssh/id_ed25519" ]; then
  ssh-keygen -t ed25519 -N '' -f "$home/.ssh/id_ed25519" -C "$u@$(hostname)" >/dev/null
fi
chown -R "$u:$u" "$home/.ssh" 2>/dev/null || true
cat "$home/.ssh/id_ed25519.pub""#
    )
}

/// Shell script (phase 2): read public keys from stdin (one per line) and append
/// any that are missing from the mesh user's authorized_keys. Prints
/// `MESH_ADDED=<n>`.
fn distribute_script(user: &str) -> String {
    let u = shell_quote(user);
    format!(
        r#"set -u
u={u}
home=$(getent passwd "$u" 2>/dev/null | cut -d: -f6 || true)
if [ -z "$home" ]; then echo "mesh: user $u not found" >&2; exit 3; fi
install -d -m 700 "$home/.ssh"
ak="$home/.ssh/authorized_keys"
touch "$ak"
added=0
while IFS= read -r key; do
  [ -z "$key" ] && continue
  if ! grep -qxF -- "$key" "$ak"; then printf '%s\n' "$key" >> "$ak"; added=$((added+1)); fi
done
chmod 700 "$home/.ssh"
chmod 600 "$ak"
chown "$u:$u" "$home/.ssh" "$ak" 2>/dev/null || true
echo "MESH_ADDED=$added""#
    )
}

/// Shell script (phase 3): ssh-keyscan the given hosts (ed25519) and merge new
/// lines into the mesh user's known_hosts. Prints `MESH_KH_ADDED=<n>`.
fn keyscan_script(user: &str, hosts: &[String]) -> String {
    let u = shell_quote(user);
    let host_args = hosts
        .iter()
        .map(|h| shell_quote(h))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        r#"set -u
u={u}
home=$(getent passwd "$u" 2>/dev/null | cut -d: -f6 || true)
if [ -z "$home" ]; then echo "mesh: user $u not found" >&2; exit 3; fi
install -d -m 700 "$home/.ssh"
kh="$home/.ssh/known_hosts"
touch "$kh"
before=$(wc -l < "$kh")
for h in {host_args}; do
  ssh-keyscan -t ed25519 "$h" 2>/dev/null >> "$kh".scan || true
done
if [ -f "$kh".scan ]; then
  sort -u "$kh" "$kh".scan > "$kh".merged && mv "$kh".merged "$kh" && rm -f "$kh".scan
fi
chown "$u:$u" "$kh" 2>/dev/null || true
after=$(wc -l < "$kh")
echo "MESH_KH_ADDED=$((after-before))""#
    )
}

/// Parse a `TAG=<integer>` line out of command stdout (e.g. `MESH_ADDED=3`).
/// Returns 0 if the tag is absent or unparseable.
fn parse_tagged_count(stdout: &str, tag: &str) -> usize {
    let prefix = format!("{tag}=");
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix(&prefix))
        .and_then(|n| n.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_script_quotes_user_and_generates_ed25519() {
        let s = keygen_script("root");
        assert!(s.contains("u='root'"));
        assert!(s.contains("getent passwd"));
        assert!(s.contains("ssh-keygen -t ed25519 -N ''"));
        assert!(s.contains("cat \"$home/.ssh/id_ed25519.pub\""));
    }

    #[test]
    fn keygen_script_escapes_adversarial_user() {
        // A user string with a quote must not break out of the shell literal.
        let s = keygen_script("ro'ot");
        assert!(s.contains(r#"u='ro'\''ot'"#));
    }

    #[test]
    fn distribute_script_appends_missing_keys_idempotently() {
        let s = distribute_script("root");
        assert!(s.contains("authorized_keys"));
        assert!(s.contains("grep -qxF"));
        assert!(s.contains("MESH_ADDED=$added"));
        assert!(s.contains("chmod 600"));
    }

    #[test]
    fn keyscan_script_quotes_each_host() {
        let s = keyscan_script("root", &["orange1".into(), "orange2".into()]);
        assert!(s.contains("ssh-keyscan -t ed25519"));
        assert!(s.contains("'orange1' 'orange2'"));
        assert!(s.contains("MESH_KH_ADDED="));
    }

    #[test]
    fn parse_tagged_count_reads_value() {
        assert_eq!(parse_tagged_count("foo\nMESH_ADDED=3\nbar", "MESH_ADDED"), 3);
        assert_eq!(parse_tagged_count("MESH_ADDED=0", "MESH_ADDED"), 0);
        assert_eq!(parse_tagged_count("no tag here", "MESH_ADDED"), 0);
        assert_eq!(parse_tagged_count("MESH_KH_ADDED=5", "MESH_KH_ADDED"), 5);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib mesh::tests 2>&1 | tail -20`
Expected: compile error or test FAIL — but actually with the code above present they should pass. To honor TDD, first paste ONLY the `#[cfg(test)] mod tests` block plus empty stub fns:

```rust
fn keygen_script(_user: &str) -> String { String::new() }
fn distribute_script(_user: &str) -> String { String::new() }
fn keyscan_script(_user: &str, _hosts: &[String]) -> String { String::new() }
fn parse_tagged_count(_stdout: &str, _tag: &str) -> usize { 0 }
```
Run: `cargo test --lib mesh::tests 2>&1 | tail -20`
Expected: FAIL (assertions on empty strings / wrong counts).

- [ ] **Step 4: Replace the stubs with the real implementations from Step 2, run again**

Run: `cargo test --lib mesh::tests 2>&1 | tail -20`
Expected: PASS (5 tests).

- [ ] **Step 5: Confirm no clippy/format regressions**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: clean. (The helpers are currently `dead_code` until Task 2 wires them; add `#[allow(dead_code)]` on each of the four fns to keep clippy green, and remove the attributes in Task 2 Step 4.)

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/mesh.rs
git commit -m "mesh: pure shell-script + parse helpers with tests"
```

---

## Task 2: MeshBuilder, result types, and run() orchestration

**Files:**
- Modify: `src/mesh.rs` (add types + builder + `run`)
- Modify: `src/lib.rs:48-52` (re-exports) and add `Dispatch::mesh`

- [ ] **Step 1: Add the public result types to `src/mesh.rs`** (top of file, after the `use`)

```rust
use crate::error::Result;
use crate::Dispatch;
use std::collections::BTreeMap;

/// Per-host outcome of a mesh bootstrap.
#[derive(Debug, Clone, Default)]
pub struct MeshHostResult {
    /// The host's collected public key (None if phase 1 failed).
    pub public_key: Option<String>,
    /// New authorized_keys lines written for this host on this run.
    pub keys_added: usize,
    /// New known_hosts lines written (0 unless `also_known_hosts`).
    pub known_hosts_added: usize,
    /// First error encountered for this host, if any.
    pub error: Option<String>,
}

/// Aggregate result of a mesh bootstrap, keyed by host.
#[derive(Debug, Clone)]
pub struct MeshResult {
    pub hosts: BTreeMap<String, MeshHostResult>,
}

impl MeshResult {
    /// True if every host completed without error.
    pub fn all_success(&self) -> bool {
        self.hosts.values().all(|h| h.error.is_none())
    }

    /// Hosts that hit an error during bootstrap.
    pub fn failed_hosts(&self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|(_, h)| h.error.is_some())
            .map(|(name, _)| name.clone())
            .collect()
    }
}
```

- [ ] **Step 2: Add the `MeshBuilder` and `run()`** (after the result types)

```rust
/// Builder for [`Dispatch::mesh`]. Establishes passwordless SSH trust for
/// [`MeshBuilder::user`] (default `root`) across the targeted hosts.
pub struct MeshBuilder<'a> {
    dispatch: &'a Dispatch,
    patterns: Vec<String>,
    user: String,
    also_known_hosts: bool,
    parallel: usize,
}

impl<'a> MeshBuilder<'a> {
    pub(crate) fn new(dispatch: &'a Dispatch, patterns: Vec<String>) -> Self {
        Self {
            parallel: dispatch.config.parallel,
            dispatch,
            patterns,
            user: "root".to_string(),
            also_known_hosts: false,
        }
    }

    /// Mesh target user whose `~/.ssh/authorized_keys` is populated. Default `root`.
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    /// Also `ssh-keyscan` peers into the mesh user's `known_hosts`. Default false.
    pub fn also_known_hosts(mut self, yes: bool) -> Self {
        self.also_known_hosts = yes;
        self
    }

    /// Max hosts processed concurrently per phase.
    pub fn parallel(mut self, n: usize) -> Self {
        self.parallel = n.max(1);
        self
    }

    /// Resolve hosts and run the bootstrap. Per-host failures are reported in
    /// [`MeshResult`]; `Err` is only returned for setup failures (e.g. no hosts).
    pub async fn run(self) -> Result<MeshResult> {
        let hosts = self.dispatch.inventory.resolve(&self.patterns)?;
        let mut out: BTreeMap<String, MeshHostResult> = hosts
            .iter()
            .map(|h| (h.clone(), MeshHostResult::default()))
            .collect();

        // Phase 1: ensure keypair + collect public keys.
        let keygen = self
            .dispatch
            .exec(hosts.clone(), keygen_script(&self.user))
            .parallel(self.parallel)
            .run()
            .await?;
        let mut good: Vec<String> = Vec::new();
        for (host, r) in &keygen.hosts {
            let entry = out.get_mut(host).expect("host present");
            if r.success && !r.stdout.trim().is_empty() {
                entry.public_key = Some(r.stdout.trim().to_string());
                good.push(host.clone());
            } else {
                entry.error = Some(format!(
                    "keygen failed (exit {}): {}",
                    r.exit_code,
                    first_nonempty(&r.error, &r.stderr)
                ));
            }
        }

        if good.is_empty() {
            return Ok(MeshResult { hosts: out });
        }

        // Combined key set distributed to every good host (full mesh, incl. self).
        let combined = good
            .iter()
            .filter_map(|h| out.get(h).and_then(|e| e.public_key.clone()))
            .collect::<Vec<_>>()
            .join("\n");

        // Phase 2: distribute authorized_keys idempotently.
        let dist = self
            .dispatch
            .exec(good.clone(), distribute_script(&self.user))
            .input(combined)
            .parallel(self.parallel)
            .run()
            .await?;
        for (host, r) in &dist.hosts {
            let entry = out.get_mut(host).expect("host present");
            if r.success {
                entry.keys_added = parse_tagged_count(&r.stdout, "MESH_ADDED");
            } else {
                entry.error = Some(format!(
                    "distribute failed (exit {}): {}",
                    r.exit_code,
                    first_nonempty(&r.error, &r.stderr)
                ));
            }
        }

        // Phase 3 (optional): known_hosts.
        if self.also_known_hosts {
            let scan = self
                .dispatch
                .exec(good.clone(), keyscan_script(&self.user, &hosts))
                .parallel(self.parallel)
                .run()
                .await?;
            for (host, r) in &scan.hosts {
                let entry = out.get_mut(host).expect("host present");
                if r.success {
                    entry.known_hosts_added = parse_tagged_count(&r.stdout, "MESH_KH_ADDED");
                } else if entry.error.is_none() {
                    entry.error = Some(format!(
                        "keyscan failed (exit {}): {}",
                        r.exit_code,
                        first_nonempty(&r.error, &r.stderr)
                    ));
                }
            }
        }

        Ok(MeshResult { hosts: out })
    }
}

fn first_nonempty(err: &Option<String>, stderr: &str) -> String {
    match err {
        Some(e) if !e.is_empty() => e.clone(),
        _ => stderr.trim().to_string(),
    }
}
```

- [ ] **Step 3: Wire `Dispatch::mesh` and re-exports in `src/lib.rs`**

Add to the re-export block (currently lines 48-52):

```rust
pub use mesh::{MeshBuilder, MeshHostResult, MeshResult};
```

Add this method to `impl Dispatch` (after `read`, before the closing brace ~line 157):

```rust
    /// Establish passwordless SSH trust for a target user (default `root`)
    /// across every host matched by `patterns`. See [`MeshBuilder`].
    pub fn mesh(
        &self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> MeshBuilder<'_> {
        MeshBuilder::new(self, collect(patterns))
    }
```

- [ ] **Step 4: Remove the `#[allow(dead_code)]` attributes** added in Task 1 Step 5 (the helpers are now used by `run`).

- [ ] **Step 5: Build, lint, test**

Run: `cargo build && cargo clippy --all-targets -- -D warnings && cargo test --lib 2>&1 | tail -15`
Expected: builds clean, clippy clean, all unit tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/mesh.rs src/lib.rs
git commit -m "mesh: MeshBuilder + run() orchestration; Dispatch::mesh"
```

---

## Task 3: Docs — README example + CHANGELOG

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add a usage example to `README.md`** (after the `client.fetch(...)` example block, around line 89)

````markdown
### Bootstrap a passwordless SSH mesh

Establish `root <-> root` trust across a set of hosts (e.g. so cluster nodes can
reach each other after a re-image). Connects as the configured transport user
(with `sudo` if set) and sets up the mesh for the target user:

```rust
let res = client
    .mesh(["orange1", "orange2", "orange3"])
    .user("root")            // mesh target user; default "root"
    .also_known_hosts(true)  // also pre-populate known_hosts
    .run()
    .await?;

for (host, r) in &res.hosts {
    if let Some(err) = &r.error {
        eprintln!("[{host}] mesh failed: {err}");
    } else {
        println!("[{host}] +{} authorized_keys", r.keys_added);
    }
}
assert!(res.all_success());
```

Idempotent: existing keypairs are reused and only missing `authorized_keys`
lines are appended, so re-running is a no-op.
````

- [ ] **Step 2: Add a `CHANGELOG.md` entry** (new section at the top, under the title)

```markdown
## Unreleased

- Add `Dispatch::mesh(...)` — bootstrap a passwordless SSH mesh for a target
  user (default `root`) across hosts. Ensures an ed25519 keypair per host,
  idempotently distributes `authorized_keys`, and optionally populates
  `known_hosts`. Honors the client's `sudo` setting.
```

- [ ] **Step 3: Verify docs build**

Run: `cargo doc --no-deps 2>&1 | tail -5`
Expected: no warnings about the new public items.

- [ ] **Step 4: Commit**

```bash
git add README.md CHANGELOG.md
git commit -m "docs: README example + CHANGELOG for mesh bootstrap"
```

---

## Task 4: Integration verification against the orange cluster

This crate has no SSH mock harness, so the orchestration is verified against
real hosts. The orange test cluster (orange1/2/3 = 192.168.123.214/215/216,
reachable as `liliang` + passwordless sudo) already has a working root mesh from
earlier manual setup, so this primarily checks idempotency + the API shape.

- [ ] **Step 1: Write a throwaway example binary**

Create `examples/mesh_smoke.rs`:

```rust
use dispatch::{Config, Dispatch, HostKeyChecking};
use std::path::PathBuf;
use std::time::Duration;

#[tokio::main]
async fn main() -> dispatch::Result<()> {
    let cfg = Config {
        ssh_config_path: Some(PathBuf::from("/dev/null")),
        config_path: Some(PathBuf::from("/dev/null")),
        host_key_checking: HostKeyChecking::AcceptAny,
        known_hosts_file: Some(PathBuf::from("/dev/null")),
        connect_timeout: Some(Duration::from_secs(5)),
        user: Some("liliang".to_string()),
        sudo: true, // connect as liliang, sudo to write /root/.ssh
        ..Default::default()
    };
    let client = Dispatch::new(cfg)?;
    let res = client
        .mesh([
            "ssh://liliang@192.168.123.214:22",
            "ssh://liliang@192.168.123.215:22",
            "ssh://liliang@192.168.123.216:22",
        ])
        .user("root")
        .also_known_hosts(true)
        .run()
        .await?;
    for (host, r) in &res.hosts {
        println!(
            "{host}: err={:?} keys_added={} kh_added={} pubkey={}",
            r.error,
            r.keys_added,
            r.known_hosts_added,
            r.public_key.as_deref().unwrap_or("-")
        );
    }
    println!("all_success={}", res.all_success());
    Ok(())
}
```

- [ ] **Step 2: Run it (twice) to confirm success + idempotency**

Run: `cargo run --example mesh_smoke`
Expected (first run): every host `err=None`, a `pubkey=ssh-ed25519 ...`,
`all_success=true`. `keys_added` may be 0 (mesh already present) or up to 3.

Run again: `cargo run --example mesh_smoke`
Expected (second run): `keys_added=0` for every host (idempotent), `all_success=true`.

- [ ] **Step 3: Independently verify trust works**

Run: `ssh -o BatchMode=yes liliang@192.168.123.214 'sudo -n ssh -o BatchMode=yes -o StrictHostKeyChecking=yes root@192.168.123.215 hostname'`
Expected: prints `orange2` (root mesh + known_hosts both in place).

- [ ] **Step 4: Remove the throwaway example and commit**

```bash
git rm examples/mesh_smoke.rs
git commit -m "chore: drop mesh smoke example after verification"
```

(Keep the example only if the repo wants a permanent `examples/` entry; default is to remove it.)

---

## Self-Review

- **Spec coverage:** API/builder (Task 2), transport-vs-mesh user via `sudo` + `.user()` (Task 2), three phases (Tasks 1+2), `MeshResult`/`MeshHostResult` (Task 2), idempotency (distribute_script grep -qxF; Task 1 + Task 4 Step 2), per-host errors not aborting (`run()` records into `out`), known_hosts optional (Task 2 phase 3), docs (Task 3), edge cases — user-not-found (`exit 3` in scripts → per-host error), unreachable host (exec marks failure → recorded), partial mesh (`good` subset). All covered.
- **Placeholder scan:** none — every code/step is concrete.
- **Type consistency:** `keygen_script`/`distribute_script`/`keyscan_script`/`parse_tagged_count` signatures match between Task 1 and Task 2 usage; `MeshBuilder`/`MeshResult`/`MeshHostResult` field names (`public_key`, `keys_added`, `known_hosts_added`, `error`, `hosts`) are consistent across definition, `run()`, README, and the example.
