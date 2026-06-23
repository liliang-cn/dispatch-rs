//! `dispatch` — a small SSH batch-operation library.
//!
//! Run commands and manage files across many servers in parallel, reusing your
//! existing `~/.ssh/config`. It wraps the system `ssh`/`scp`, so host aliases,
//! wildcard `Host` blocks, `IdentityFile`, `known_hosts`, `ProxyJump` and
//! `ControlMaster` all behave exactly as your ssh client is already configured.
//!
//! ```no_run
//! use dispatch::{Config, Dispatch};
//! use std::time::Duration;
//!
//! # async fn run() -> dispatch::Result<()> {
//! let client = Dispatch::new(Config::default())?; // reads ~/.ssh/config
//!
//! // Patterns: aliases, groups (~/.dispatch/config.toml), wildcards, or IPs.
//! let res = client
//!     .exec(["orange1", "orange*"], "uptime")
//!     .parallel(5)
//!     .timeout(Duration::from_secs(30))
//!     .run()
//!     .await?;
//!
//! for (host, r) in &res.hosts {
//!     if r.success {
//!         println!("[{host}] {}", r.stdout.trim());
//!     } else {
//!         eprintln!("[{host}] exit {} {}", r.exit_code, r.stderr.trim());
//!     }
//! }
//! assert!(res.all_success());
//!
//! client.copy(["web"], "./app.conf", "/etc/app.conf").run().await?;
//! client.fetch(["web"], "/var/log/app.log", "./logs").run().await?;
//! # Ok(()) }
//! ```

mod config;
mod conn;
mod error;
mod exec;
mod inventory;
mod mesh;
mod transfer;

pub use config::{Config, HostKeyChecking};
pub use error::{Error, Result};
pub use exec::{ExecBuilder, ExecResult, HostResult, StreamCallback, StreamType};
pub use inventory::Inventory;
pub use transfer::{
    CopyBuilder, FetchBuilder, ProgressCallback, ReadBuilder, ReadHostResult, ReadResult,
    TransferHostResult, TransferResult, UpdateBuilder, UpdateHostResult, UpdatePhase, UpdateResult,
    WriteBuilder,
};

use conn::Conn;

/// The dispatch client: holds configuration and the resolved inventory.
pub struct Dispatch {
    pub(crate) config: Config,
    pub(crate) inventory: Inventory,
    pub(crate) conn: Conn,
}

impl Dispatch {
    /// Create a client, loading groups from `~/.dispatch/config.toml` (if any)
    /// and host aliases from `~/.ssh/config`.
    pub fn new(config: Config) -> Result<Self> {
        let inventory = Inventory::load(
            config.ssh_config_path.as_deref(),
            config.config_path.as_deref(),
        )?;
        let conn = Conn::from_config(&config);
        Ok(Self {
            config,
            inventory,
            conn,
        })
    }

    /// Build directly from a pre-loaded [`Inventory`].
    pub fn with_inventory(config: Config, inventory: Inventory) -> Self {
        let conn = Conn::from_config(&config);
        Self {
            config,
            inventory,
            conn,
        }
    }

    /// Resolve patterns (aliases, groups, wildcards, IPs) into concrete hosts.
    pub fn resolve(&self, patterns: &[String]) -> Result<Vec<String>> {
        self.inventory.resolve(patterns)
    }

    /// Access the underlying inventory (e.g. to list known hosts).
    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    /// Run `command` on every host matched by `patterns`.
    pub fn exec(
        &self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
        command: impl Into<String>,
    ) -> ExecBuilder<'_> {
        ExecBuilder::new(self, collect(patterns), command.into())
    }

    /// Push a local file/dir to `dest` on each matched host.
    pub fn copy(
        &self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
        src: impl Into<std::path::PathBuf>,
        dest: impl Into<String>,
    ) -> CopyBuilder<'_> {
        CopyBuilder::new(self, collect(patterns), src, dest)
    }

    /// Fetch `src` from each matched host into `dest/<host>/`.
    pub fn fetch(
        &self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
        src: impl Into<String>,
        dest: impl Into<std::path::PathBuf>,
    ) -> FetchBuilder<'_> {
        FetchBuilder::new(self, collect(patterns), src, dest)
    }

    /// Update a remote file from `src`: skips hosts whose file already matches
    /// (sha256), with optional backup and file mode. See [`UpdateBuilder`].
    pub fn update(
        &self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
        src: impl Into<std::path::PathBuf>,
        dest: impl Into<String>,
    ) -> UpdateBuilder<'_> {
        UpdateBuilder::new(self, collect(patterns), src, dest)
    }

    /// Write in-memory `content` to `dest` on each matched host, creating parent
    /// directories first. Honors the client's `sudo` setting; see [`WriteBuilder`].
    pub fn write(
        &self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
        content: impl Into<Vec<u8>>,
        dest: impl Into<String>,
    ) -> WriteBuilder<'_> {
        WriteBuilder::new(self, collect(patterns), content.into(), dest)
    }

    /// Read `path` from each matched host, returning its bytes. See [`ReadBuilder`].
    pub fn read(
        &self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
        path: impl Into<String>,
    ) -> ReadBuilder<'_> {
        ReadBuilder::new(self, collect(patterns), path.into())
    }
}

fn collect(patterns: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    patterns.into_iter().map(Into::into).collect()
}
