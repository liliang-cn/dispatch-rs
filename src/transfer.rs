//! Batch file transfer (push / pull / update) via the system `scp` + ssh.

use crate::error::{Error, Result};
use crate::exec::{exec_via_ssh, shell_quote};
use crate::Dispatch;
use futures::stream::{self, StreamExt};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

/// Per-host transfer outcome.
#[derive(Debug, Clone)]
pub struct TransferHostResult {
    pub host: String,
    pub success: bool,
    /// Local destination (for `fetch`) or remote destination (for `copy`).
    pub dest: String,
    pub error: Option<String>,
}

/// Aggregate transfer result keyed by host.
#[derive(Debug, Clone)]
pub struct TransferResult {
    pub hosts: BTreeMap<String, TransferHostResult>,
}

impl TransferResult {
    pub fn all_success(&self) -> bool {
        self.hosts.values().all(|r| r.success)
    }

    pub fn failed_hosts(&self) -> Vec<String> {
        self.hosts
            .values()
            .filter(|r| !r.success)
            .map(|r| r.host.clone())
            .collect()
    }
}

/// Builder for [`Dispatch::copy`] (push a local path to each host).
pub struct CopyBuilder<'a> {
    dispatch: &'a Dispatch,
    patterns: Vec<String>,
    src: PathBuf,
    dest: String,
    parallel: usize,
    recursive: bool,
}

impl<'a> CopyBuilder<'a> {
    pub(crate) fn new(
        dispatch: &'a Dispatch,
        patterns: Vec<String>,
        src: impl Into<PathBuf>,
        dest: impl Into<String>,
    ) -> Self {
        Self {
            parallel: dispatch.config.parallel,
            dispatch,
            patterns,
            src: src.into(),
            dest: dest.into(),
            recursive: true,
        }
    }

    pub fn parallel(mut self, n: usize) -> Self {
        self.parallel = n.max(1);
        self
    }

    /// Copy directories recursively (`scp -r`). Default: true.
    pub fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    pub async fn run(self) -> Result<TransferResult> {
        let hosts = self.dispatch.inventory.resolve(&self.patterns)?;
        let src = self.src;
        let dest = self.dest;
        let recursive = self.recursive;

        let results = stream::iter(hosts)
            .map(|host| {
                let src = src.clone();
                let dest = dest.clone();
                async move {
                    let res = scp_push(&host, &src, &dest, recursive).await;
                    (host.clone(), dest, res)
                }
            })
            .buffer_unordered(self.parallel)
            .collect::<Vec<_>>()
            .await;

        Ok(collect(results))
    }
}

/// Builder for [`Dispatch::fetch`] (pull a remote path from each host).
///
/// Each host's file lands under `dest/<host>/` to avoid collisions.
pub struct FetchBuilder<'a> {
    dispatch: &'a Dispatch,
    patterns: Vec<String>,
    src: String,
    dest: PathBuf,
    parallel: usize,
    recursive: bool,
}

impl<'a> FetchBuilder<'a> {
    pub(crate) fn new(
        dispatch: &'a Dispatch,
        patterns: Vec<String>,
        src: impl Into<String>,
        dest: impl Into<PathBuf>,
    ) -> Self {
        Self {
            parallel: dispatch.config.parallel,
            dispatch,
            patterns,
            src: src.into(),
            dest: dest.into(),
            recursive: true,
        }
    }

    pub fn parallel(mut self, n: usize) -> Self {
        self.parallel = n.max(1);
        self
    }

    pub fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    pub async fn run(self) -> Result<TransferResult> {
        let hosts = self.dispatch.inventory.resolve(&self.patterns)?;
        let src = self.src;
        let dest = self.dest;
        let recursive = self.recursive;

        let results = stream::iter(hosts)
            .map(|host| {
                let src = src.clone();
                let host_dest = dest.join(&host);
                async move {
                    let label = host_dest.display().to_string();
                    let res = scp_pull(&host, &src, &host_dest, recursive).await;
                    (host.clone(), label, res)
                }
            })
            .buffer_unordered(self.parallel)
            .collect::<Vec<_>>()
            .await;

        Ok(collect(results))
    }
}

fn collect(results: Vec<(String, String, Result<()>)>) -> TransferResult {
    let mut map = BTreeMap::new();
    for (host, dest, res) in results {
        let hr = match res {
            Ok(()) => TransferHostResult {
                host: host.clone(),
                success: true,
                dest,
                error: None,
            },
            Err(e) => TransferHostResult {
                host: host.clone(),
                success: false,
                dest,
                error: Some(e.to_string()),
            },
        };
        map.insert(host, hr);
    }
    TransferResult { hosts: map }
}

async fn scp_push(host: &str, src: &Path, dest: &str, recursive: bool) -> Result<()> {
    let mut cmd = base_scp(recursive);
    cmd.arg(src).arg(format!("{host}:{dest}"));
    run_status(cmd, "scp push").await
}

async fn scp_pull(host: &str, src: &str, dest_dir: &Path, recursive: bool) -> Result<()> {
    std::fs::create_dir_all(dest_dir)?;
    let mut cmd = base_scp(recursive);
    cmd.arg(format!("{host}:{src}")).arg(dest_dir);
    run_status(cmd, "scp pull").await
}

fn base_scp(recursive: bool) -> Command {
    let mut cmd = Command::new("scp");
    // Non-interactive; rely on ~/.ssh/config for everything else.
    cmd.arg("-o").arg("BatchMode=yes");
    if recursive {
        cmd.arg("-r");
    }
    cmd
}

async fn run_status(mut cmd: Command, what: &str) -> Result<()> {
    let output = cmd.output().await?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(Error::Config(format!(
            "{what} failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        )))
    }
}

// ----------------------------------------------------------------------------
// Update: like copy, but skips hosts whose file already matches (sha256), with
// an optional backup of the existing remote file and an optional file mode.
// ----------------------------------------------------------------------------

/// Per-host update outcome.
#[derive(Debug, Clone)]
pub struct UpdateHostResult {
    pub host: String,
    pub success: bool,
    /// Remote file already matched; nothing was transferred.
    pub skipped: bool,
    /// Bytes written (0 when skipped).
    pub bytes_copied: u64,
    pub error: Option<String>,
}

/// Aggregate update result keyed by host.
#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub hosts: BTreeMap<String, UpdateHostResult>,
}

impl UpdateResult {
    pub fn all_success(&self) -> bool {
        self.hosts.values().all(|r| r.success)
    }

    pub fn failed_hosts(&self) -> Vec<String> {
        self.hosts
            .values()
            .filter(|r| !r.success)
            .map(|r| r.host.clone())
            .collect()
    }

    /// Hosts that were already up to date.
    pub fn skipped_hosts(&self) -> Vec<String> {
        self.hosts
            .values()
            .filter(|r| r.skipped)
            .map(|r| r.host.clone())
            .collect()
    }
}

/// Builder for [`Dispatch::update`].
pub struct UpdateBuilder<'a> {
    dispatch: &'a Dispatch,
    patterns: Vec<String>,
    src: PathBuf,
    dest: String,
    parallel: usize,
    backup: bool,
    mode: Option<u32>,
    timeout: Duration,
}

impl<'a> UpdateBuilder<'a> {
    pub(crate) fn new(
        dispatch: &'a Dispatch,
        patterns: Vec<String>,
        src: impl Into<PathBuf>,
        dest: impl Into<String>,
    ) -> Self {
        Self {
            parallel: dispatch.config.parallel,
            timeout: dispatch.config.timeout,
            dispatch,
            patterns,
            src: src.into(),
            dest: dest.into(),
            backup: false,
            mode: None,
        }
    }

    pub fn parallel(mut self, n: usize) -> Self {
        self.parallel = n.max(1);
        self
    }

    /// Back up the existing remote file to `<dest>.bak` before overwriting.
    pub fn backup(mut self, backup: bool) -> Self {
        self.backup = backup;
        self
    }

    /// `chmod` the file to this (octal) mode after writing, e.g. `0o644`.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    pub async fn run(self) -> Result<UpdateResult> {
        let hosts = self.dispatch.inventory.resolve(&self.patterns)?;

        let bytes = std::fs::read(&self.src)?;
        let size = bytes.len() as u64;
        let local_sum = format!("{:x}", Sha256::digest(&bytes));

        let src = self.src;
        let dest = self.dest;
        let backup = self.backup;
        let mode = self.mode;
        let timeout = self.timeout;

        let results = stream::iter(hosts)
            .map(|host| {
                let src = src.clone();
                let dest = dest.clone();
                let local_sum = local_sum.clone();
                async move {
                    let res =
                        update_one(&host, &src, &dest, &local_sum, size, backup, mode, timeout)
                            .await;
                    (host, res)
                }
            })
            .buffer_unordered(self.parallel)
            .collect::<Vec<_>>()
            .await;

        let mut map = BTreeMap::new();
        for (host, res) in results {
            let hr = match res {
                Ok(hr) => hr,
                Err(e) => UpdateHostResult {
                    host: host.clone(),
                    success: false,
                    skipped: false,
                    bytes_copied: 0,
                    error: Some(e.to_string()),
                },
            };
            map.insert(host, hr);
        }
        Ok(UpdateResult { hosts: map })
    }
}

#[allow(clippy::too_many_arguments)]
async fn update_one(
    host: &str,
    src: &Path,
    dest: &str,
    local_sum: &str,
    size: u64,
    backup: bool,
    mode: Option<u32>,
    timeout: Duration,
) -> Result<UpdateHostResult> {
    let qdest = shell_quote(dest);

    // 1. Remote checksum (echo -1 when the file is absent).
    let check = format!(
        "if [ -f {q} ]; then sha256sum {q} 2>/dev/null | cut -d' ' -f1; else echo -1; fi",
        q = qdest
    );
    let remote = tokio::time::timeout(timeout, exec_via_ssh(host, &check, None, None))
        .await
        .map_err(|_| Error::Config(format!("checksum on {host} timed out")))??;
    let remote_sum = remote.stdout.trim();

    if remote_sum == local_sum {
        return Ok(UpdateHostResult {
            host: host.to_string(),
            success: true,
            skipped: true,
            bytes_copied: 0,
            error: None,
        });
    }

    // 2. Optional backup of the existing file.
    if backup {
        let backup_cmd = format!("if [ -f {q} ]; then cp {q} {q}.bak; fi", q = qdest);
        let r = exec_via_ssh(host, &backup_cmd, None, None).await?;
        if !r.success {
            return Err(Error::Config(format!("backup failed: {}", r.stderr.trim())));
        }
    }

    // 3. Push the new file.
    scp_push(host, src, dest, true).await?;

    // 4. Optional chmod.
    if let Some(m) = mode {
        let chmod = format!("chmod {:o} {}", m, qdest);
        let r = exec_via_ssh(host, &chmod, None, None).await?;
        if !r.success {
            return Err(Error::Config(format!("chmod failed: {}", r.stderr.trim())));
        }
    }

    Ok(UpdateHostResult {
        host: host.to_string(),
        success: true,
        skipped: false,
        bytes_copied: size,
        error: None,
    })
}
