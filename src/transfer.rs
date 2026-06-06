//! Batch file transfer (push / pull / update) via the system `scp` + ssh.

use crate::conn::Conn;
use crate::error::{Error, Result};
use crate::exec::{exec_via_ssh, run_on_session, shell_quote};
use crate::Dispatch;
use base64::Engine;
use futures::stream::{self, StreamExt};
use openssh::Session;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Parent directory of a remote (unix) path, e.g. `/a/b/c` -> `/a/b`.
fn remote_parent(path: &str) -> Option<&str> {
    path.rfind('/')
        .map(|i| if i == 0 { "/" } else { &path[..i] })
}

/// Phases reported by [`UpdateBuilder::progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    /// Comparing the local and remote checksums.
    Checking,
    /// Remote file already matched; nothing to do.
    Skipped,
    /// Backing up the existing remote file.
    BackingUp,
    /// Transferring the new file.
    Copying,
    /// Finished for this host.
    Done,
}

/// Callback invoked as each host moves through the update phases.
pub type ProgressCallback = Arc<dyn Fn(&str, UpdatePhase) + Send + Sync>;

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
        let conn = self.dispatch.conn.clone();

        let results = stream::iter(hosts)
            .map(|host| {
                let src = src.clone();
                let dest = dest.clone();
                let conn = conn.clone();
                async move {
                    let res = scp_push_mkdir(&host, &conn, &src, &dest, recursive).await;
                    (host.clone(), dest, res)
                }
            })
            .buffer_unordered(self.parallel)
            .collect::<Vec<_>>()
            .await;

        Ok(collect(results))
    }
}

/// `scp` push, but create the destination's parent directory first so the copy
/// never fails with "No such file or directory".
async fn scp_push_mkdir(
    host: &str,
    conn: &Conn,
    src: &Path,
    dest: &str,
    recursive: bool,
) -> Result<()> {
    if let Some(parent) = remote_parent(dest) {
        let mkdir = format!("mkdir -p {}", shell_quote(parent));
        let r = crate::exec::exec_via_ssh(host, conn, &mkdir, None, None).await?;
        if !r.success {
            return Err(Error::Config(format!("mkdir failed: {}", r.stderr.trim())));
        }
    }
    scp_push(host, src, dest, recursive).await
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
    progress: Option<ProgressCallback>,
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
            progress: None,
        }
    }

    /// Observe each host's progress through the update phases.
    pub fn progress<F>(mut self, cb: F) -> Self
    where
        F: Fn(&str, UpdatePhase) + Send + Sync + 'static,
    {
        self.progress = Some(Arc::new(cb));
        self
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
        let progress = self.progress;
        let conn = self.dispatch.conn.clone();

        let results = stream::iter(hosts)
            .map(|host| {
                let src = src.clone();
                let dest = dest.clone();
                let local_sum = local_sum.clone();
                let progress = progress.clone();
                let conn = conn.clone();
                async move {
                    let res = update_one(
                        &host,
                        &conn,
                        &src,
                        &dest,
                        &local_sum,
                        size,
                        backup,
                        mode,
                        timeout,
                        progress.as_ref(),
                    )
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
    conn: &Conn,
    src: &Path,
    dest: &str,
    local_sum: &str,
    size: u64,
    backup: bool,
    mode: Option<u32>,
    timeout: Duration,
    progress: Option<&ProgressCallback>,
) -> Result<UpdateHostResult> {
    let emit = |phase: UpdatePhase| {
        if let Some(cb) = progress {
            cb(host, phase);
        }
    };
    let qdest = shell_quote(dest);

    // One ssh connection reused for checksum / backup / chmod on this host.
    let session = tokio::time::timeout(timeout, conn.connect(host))
        .await
        .map_err(|_| Error::Config(format!("connect to {host} timed out")))??;

    let result = update_on_session(
        &session, conn, host, src, dest, &qdest, local_sum, size, backup, mode, &emit,
    )
    .await;

    let _ = session.close().await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn update_on_session(
    session: &Session,
    conn: &Conn,
    host: &str,
    src: &Path,
    dest: &str,
    qdest: &str,
    local_sum: &str,
    size: u64,
    backup: bool,
    mode: Option<u32>,
    emit: &dyn Fn(UpdatePhase),
) -> Result<UpdateHostResult> {
    // 1. Remote checksum (echo -1 when the file is absent).
    emit(UpdatePhase::Checking);
    let check = format!(
        "if [ -f {q} ]; then sha256sum {q} 2>/dev/null | cut -d' ' -f1; else echo -1; fi",
        q = qdest
    );
    let remote = run_on_session(session, conn, host, &check, None, None).await?;
    if remote.stdout.trim() == local_sum {
        emit(UpdatePhase::Skipped);
        emit(UpdatePhase::Done);
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
        emit(UpdatePhase::BackingUp);
        let backup_cmd = format!("if [ -f {q} ]; then cp {q} {q}.bak; fi", q = qdest);
        let r = run_on_session(session, conn, host, &backup_cmd, None, None).await?;
        if !r.success {
            return Err(Error::Config(format!("backup failed: {}", r.stderr.trim())));
        }
    }

    // 3. Push the new file (scp uses its own connection).
    emit(UpdatePhase::Copying);
    scp_push(host, src, dest, true).await?;

    // 4. Optional chmod.
    if let Some(m) = mode {
        let chmod = format!("chmod {:o} {}", m, qdest);
        let r = run_on_session(session, conn, host, &chmod, None, None).await?;
        if !r.success {
            return Err(Error::Config(format!("chmod failed: {}", r.stderr.trim())));
        }
    }

    emit(UpdatePhase::Done);
    Ok(UpdateHostResult {
        host: host.to_string(),
        success: true,
        skipped: false,
        bytes_copied: size,
        error: None,
    })
}

// ----------------------------------------------------------------------------
// Write: write in-memory content to a remote file, creating parent dirs first
// (and honoring sudo). This is the robust primitive for pushing generated
// configs; unlike scp it never fails with "No such file or directory".
// ----------------------------------------------------------------------------

/// Builder for [`Dispatch::write`].
pub struct WriteBuilder<'a> {
    dispatch: &'a Dispatch,
    patterns: Vec<String>,
    content: Vec<u8>,
    dest: String,
    parallel: usize,
    mode: Option<u32>,
    timeout: Duration,
}

impl<'a> WriteBuilder<'a> {
    pub(crate) fn new(
        dispatch: &'a Dispatch,
        patterns: Vec<String>,
        content: Vec<u8>,
        dest: impl Into<String>,
    ) -> Self {
        Self {
            parallel: dispatch.config.parallel,
            timeout: dispatch.config.timeout,
            dispatch,
            patterns,
            content,
            dest: dest.into(),
            mode: None,
        }
    }

    pub fn parallel(mut self, n: usize) -> Self {
        self.parallel = n.max(1);
        self
    }

    /// `chmod` the file to this (octal) mode after writing, e.g. `0o600`.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    pub async fn run(self) -> Result<TransferResult> {
        let hosts = self.dispatch.inventory.resolve(&self.patterns)?;
        let conn = self.dispatch.conn.clone();
        let dest = self.dest;
        let mode = self.mode;
        let timeout = self.timeout;
        let content_b64 = Arc::new(b64(&self.content));

        let results = stream::iter(hosts)
            .map(|host| {
                let conn = conn.clone();
                let dest = dest.clone();
                let content_b64 = content_b64.clone();
                async move {
                    let res = write_one(&host, &conn, &content_b64, &dest, mode, timeout).await;
                    (host.clone(), dest, res)
                }
            })
            .buffer_unordered(self.parallel)
            .collect::<Vec<_>>()
            .await;

        Ok(collect(results))
    }
}

async fn write_one(
    host: &str,
    conn: &Conn,
    content_b64: &str,
    dest: &str,
    mode: Option<u32>,
    timeout: Duration,
) -> Result<()> {
    let qdest = shell_quote(dest);
    let mut script = String::new();
    if let Some(parent) = remote_parent(dest) {
        script.push_str(&format!("mkdir -p {} && ", shell_quote(parent)));
    }
    script.push_str(&format!(
        "printf %s {} | base64 -d > {}",
        shell_quote(content_b64),
        qdest
    ));
    if let Some(m) = mode {
        script.push_str(&format!(" && chmod {:o} {}", m, qdest));
    }

    let r = tokio::time::timeout(timeout, exec_via_ssh(host, conn, &script, None, None))
        .await
        .map_err(|_| Error::Config(format!("write to {host} timed out")))??;
    if r.success {
        Ok(())
    } else {
        Err(Error::Config(format!("write failed: {}", r.stderr.trim())))
    }
}

// ----------------------------------------------------------------------------
// Read: fetch a remote file's bytes (base64 round-trip, binary-safe).
// ----------------------------------------------------------------------------

/// Per-host read outcome.
#[derive(Debug, Clone)]
pub struct ReadHostResult {
    pub host: String,
    pub success: bool,
    pub content: Vec<u8>,
    pub error: Option<String>,
}

/// Aggregate read result keyed by host.
#[derive(Debug, Clone)]
pub struct ReadResult {
    pub hosts: BTreeMap<String, ReadHostResult>,
}

impl ReadResult {
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

/// Builder for [`Dispatch::read`].
pub struct ReadBuilder<'a> {
    dispatch: &'a Dispatch,
    patterns: Vec<String>,
    path: String,
    parallel: usize,
    timeout: Duration,
}

impl<'a> ReadBuilder<'a> {
    pub(crate) fn new(dispatch: &'a Dispatch, patterns: Vec<String>, path: String) -> Self {
        Self {
            parallel: dispatch.config.parallel,
            timeout: dispatch.config.timeout,
            dispatch,
            patterns,
            path,
        }
    }

    pub fn parallel(mut self, n: usize) -> Self {
        self.parallel = n.max(1);
        self
    }

    pub async fn run(self) -> Result<ReadResult> {
        let hosts = self.dispatch.inventory.resolve(&self.patterns)?;
        let conn = self.dispatch.conn.clone();
        let path = self.path;
        let timeout = self.timeout;

        let results = stream::iter(hosts)
            .map(|host| {
                let conn = conn.clone();
                let path = path.clone();
                async move {
                    let res = read_one(&host, &conn, &path, timeout).await;
                    (host, res)
                }
            })
            .buffer_unordered(self.parallel)
            .collect::<Vec<_>>()
            .await;

        let mut map = BTreeMap::new();
        for (host, res) in results {
            let hr = match res {
                Ok(content) => ReadHostResult {
                    host: host.clone(),
                    success: true,
                    content,
                    error: None,
                },
                Err(e) => ReadHostResult {
                    host: host.clone(),
                    success: false,
                    content: Vec::new(),
                    error: Some(e.to_string()),
                },
            };
            map.insert(host, hr);
        }
        Ok(ReadResult { hosts: map })
    }
}

async fn read_one(host: &str, conn: &Conn, path: &str, timeout: Duration) -> Result<Vec<u8>> {
    let script = format!("base64 {}", shell_quote(path));
    let r = tokio::time::timeout(timeout, exec_via_ssh(host, conn, &script, None, None))
        .await
        .map_err(|_| Error::Config(format!("read from {host} timed out")))??;
    if !r.success {
        return Err(Error::Config(format!("read failed: {}", r.stderr.trim())));
    }
    // Strip whitespace (base64 line wrapping differs across coreutils/BSD).
    let cleaned: String = r.stdout.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| Error::Config(format!("invalid base64 from {host}: {e}")))
}
