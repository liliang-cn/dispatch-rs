use std::path::PathBuf;
use std::time::Duration;

/// Client configuration.
///
/// All fields are optional with sensible defaults. Per-host connection details
/// (user, port, identity file, proxy, known_hosts) come from the system ssh and
/// your `~/.ssh/config`, so they are intentionally not duplicated here.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to the dispatch groups file. Defaults to `~/.dispatch/config.toml`
    /// when `None`; a missing file simply means "no groups".
    pub config_path: Option<PathBuf>,

    /// Path to the ssh config used for host-alias / wildcard resolution.
    /// Defaults to `~/.ssh/config` when `None`.
    pub ssh_config_path: Option<PathBuf>,

    /// Default maximum number of hosts operated on concurrently.
    pub parallel: usize,

    /// Default per-host timeout for a single operation.
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_path: None,
            ssh_config_path: None,
            parallel: 10,
            timeout: Duration::from_secs(300),
        }
    }
}
