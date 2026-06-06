//! Host resolution: turn user-supplied patterns into a concrete host list.
//!
//! Patterns may be:
//! - a group name defined in `~/.dispatch/config.toml`
//! - a wildcard (`orange*`, `web0?`) matched against `~/.ssh/config` `Host` aliases
//! - a concrete ssh alias or raw IP/hostname (passed through unchanged)

use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
struct DispatchToml {
    #[serde(default)]
    groups: BTreeMap<String, GroupDef>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GroupDef {
    /// `web = ["a", "b"]`
    List(Vec<String>),
    /// `[groups.web]` with `hosts = ["a", "b"]`
    Detailed { hosts: Vec<String> },
}

/// Resolved view of ssh-config host aliases and dispatch groups.
pub struct Inventory {
    /// Concrete host aliases from ssh config (wildcard `Host` lines excluded).
    ssh_hosts: Vec<String>,
    /// Group name -> member patterns (members may themselves be groups/wildcards).
    groups: BTreeMap<String, Vec<String>>,
}

impl Inventory {
    /// Load aliases from `ssh_config` and groups from `dispatch_config`.
    /// `None` paths fall back to the standard locations; missing files are ok.
    pub fn load(ssh_config: Option<&Path>, dispatch_config: Option<&Path>) -> Result<Self> {
        Ok(Self {
            ssh_hosts: parse_ssh_config_hosts(ssh_config)?,
            groups: parse_groups(dispatch_config)?,
        })
    }

    /// The ssh-config host aliases discovered (useful for listing/UX).
    pub fn hosts(&self) -> &[String] {
        &self.ssh_hosts
    }

    /// Resolve `patterns` into a deduped, order-preserving host list.
    pub fn resolve(&self, patterns: &[String]) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for pat in patterns {
            self.resolve_one(pat, &mut out, 0)?;
        }
        if out.is_empty() {
            return Err(Error::NoHosts);
        }
        Ok(out)
    }

    fn resolve_one(&self, pat: &str, out: &mut Vec<String>, depth: usize) -> Result<()> {
        if depth > 16 {
            return Err(Error::Config(format!(
                "group recursion too deep at '{pat}'"
            )));
        }

        // 1. group name
        if let Some(members) = self.groups.get(pat) {
            for m in members {
                self.resolve_one(m, out, depth + 1)?;
            }
            return Ok(());
        }

        // 2. wildcard against ssh-config aliases
        if pat.contains('*') || pat.contains('?') || pat.contains('[') {
            if let Ok(glob) = glob::Pattern::new(pat) {
                let mut matched = false;
                for h in &self.ssh_hosts {
                    if glob.matches(h) {
                        push_unique(out, h);
                        matched = true;
                    }
                }
                if matched {
                    return Ok(());
                }
            }
            // fall through and treat literally if nothing matched
        }

        // 3. literal alias / IP / hostname
        push_unique(out, pat);
        Ok(())
    }
}

fn push_unique(out: &mut Vec<String>, h: &str) {
    if !out.iter().any(|x| x == h) {
        out.push(h.to_string());
    }
}

fn parse_ssh_config_hosts(path: Option<&Path>) -> Result<Vec<String>> {
    let path = path.map(PathBuf::from).unwrap_or_else(default_ssh_config);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut hosts: Vec<String> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(kw) = parts.next() else { continue };
        if !kw.eq_ignore_ascii_case("Host") {
            continue;
        }
        for name in parts {
            // skip wildcard/negated patterns and the `Host *` catch-all
            if name.contains('*') || name.contains('?') || name.starts_with('!') {
                continue;
            }
            push_unique(&mut hosts, name);
        }
    }
    Ok(hosts)
}

fn parse_groups(path: Option<&Path>) -> Result<BTreeMap<String, Vec<String>>> {
    let path = path
        .map(PathBuf::from)
        .unwrap_or_else(default_dispatch_config);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let parsed: DispatchToml =
        toml::from_str(&content).map_err(|e| Error::Config(e.to_string()))?;
    let mut groups = BTreeMap::new();
    for (name, def) in parsed.groups {
        let hosts = match def {
            GroupDef::List(l) => l,
            GroupDef::Detailed { hosts } => hosts,
        };
        groups.insert(name, hosts);
    }
    Ok(groups)
}

fn default_ssh_config() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".ssh")
        .join("config")
}

fn default_dispatch_config() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".dispatch")
        .join("config.toml")
}
