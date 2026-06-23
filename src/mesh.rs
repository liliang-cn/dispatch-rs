//! SSH passwordless mesh bootstrap: establish `user <-> user` trust across hosts.

use crate::exec::shell_quote;

/// Shell script (phase 1): ensure the mesh user has an ed25519 keypair, then
/// print its public key on stdout. Honors sudo via the caller's Conn wrapping.
#[allow(dead_code)]
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
#[allow(dead_code)]
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

/// Shell script (phase 3): ssh-keyscan the given hosts (ed25519) and append any
/// new lines to the mesh user's known_hosts (existing content untouched).
/// Prints `MESH_KH_ADDED=<n>`.
#[allow(dead_code)]
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
  while IFS= read -r line; do
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
#[allow(dead_code)]
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
