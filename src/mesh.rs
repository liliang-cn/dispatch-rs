//! SSH passwordless mesh bootstrap: establish `user <-> user` trust across hosts.

use crate::error::Result;
use crate::exec::shell_quote;
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
while IFS= read -r key || [ -n "$key" ]; do
  [ -z "$key" ] && continue
  if ! grep -qxF -- "$key" "$ak"; then printf '%s\n' "$key" >> "$ak"; added=$((added+1)); fi
done
chmod 700 "$home/.ssh"
chmod 600 "$ak"
chown "$u:$u" "$home/.ssh" "$ak" 2>/dev/null || true
echo "MESH_ADDED=$added""#
    )
}

/// Shell script (phase 3): ssh-keyscan the given hosts (ed25519) and append any
/// new lines to the mesh user's known_hosts (existing content untouched).
/// Prints `MESH_KH_ADDED=<n>`.
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
rm -f "$kh".scan
for h in {host_args}; do
  ssh-keyscan -t ed25519 "$h" 2>/dev/null >> "$kh".scan || true
done
added=0
if [ -f "$kh".scan ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    [ -z "$line" ] && continue
    if ! grep -qxF -- "$line" "$kh"; then printf '%s\n' "$line" >> "$kh"; added=$((added+1)); fi
  done < "$kh".scan
  rm -f "$kh".scan
fi
chmod 600 "$kh"
chown "$u:$u" "$kh" 2>/dev/null || true
echo "MESH_KH_ADDED=$added""#
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
        // The combined keyset arrives without a trailing newline, so the read
        // loop must still process the final line (classic last-line drop).
        assert!(s.contains(r#"read -r key || [ -n "$key" ]"#));
    }

    #[test]
    fn keyscan_script_quotes_each_host() {
        let s = keyscan_script("root", &["orange1".into(), "orange2".into()]);
        assert!(s.contains("ssh-keyscan -t ed25519"));
        assert!(s.contains("'orange1' 'orange2'"));
        assert!(s.contains("MESH_KH_ADDED="));
        // appends only new lines (no global sort -u rewrite of existing file)
        assert!(s.contains("grep -qxF"));
        assert!(!s.contains("sort -u"));
        assert!(s.contains("chmod 600"));
    }

    #[test]
    fn parse_tagged_count_reads_value() {
        assert_eq!(
            parse_tagged_count("foo\nMESH_ADDED=3\nbar", "MESH_ADDED"),
            3
        );
        assert_eq!(parse_tagged_count("MESH_ADDED=0", "MESH_ADDED"), 0);
        assert_eq!(parse_tagged_count("no tag here", "MESH_ADDED"), 0);
        assert_eq!(parse_tagged_count("MESH_KH_ADDED=5", "MESH_KH_ADDED"), 5);
    }
}
