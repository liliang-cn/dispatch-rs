//! Batch command execution (with optional live streaming).

use crate::conn::Conn;
use crate::error::Result;
use crate::Dispatch;
use futures::stream::{self, StreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Which output stream a streamed chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    Stdout,
    Stderr,
}

/// Callback invoked with each chunk of output as it arrives, per host.
pub type StreamCallback = Arc<dyn Fn(&str, StreamType, &[u8]) + Send + Sync>;

/// Result of running a command on one host.
#[derive(Debug, Clone)]
pub struct HostResult {
    pub host: String,
    /// Command exited 0 and the connection succeeded.
    pub success: bool,
    /// Remote exit code (`-1` if it could not be determined).
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Connection/timeout error (set when we never got an exit code).
    pub error: Option<String>,
}

/// Aggregate result across all targeted hosts (keyed by host).
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub hosts: BTreeMap<String, HostResult>,
}

impl ExecResult {
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

/// Builder for an [`Dispatch::exec`] call.
pub struct ExecBuilder<'a> {
    dispatch: &'a Dispatch,
    patterns: Vec<String>,
    command: String,
    parallel: usize,
    timeout: Duration,
    env: BTreeMap<String, String>,
    dir: Option<String>,
    input: Option<String>,
    stream: Option<StreamCallback>,
}

impl<'a> ExecBuilder<'a> {
    pub(crate) fn new(dispatch: &'a Dispatch, patterns: Vec<String>, command: String) -> Self {
        Self {
            parallel: dispatch.config.parallel,
            timeout: dispatch.config.timeout,
            dispatch,
            patterns,
            command,
            env: BTreeMap::new(),
            dir: None,
            input: None,
            stream: None,
        }
    }

    /// Max hosts run concurrently.
    pub fn parallel(mut self, n: usize) -> Self {
        self.parallel = n.max(1);
        self
    }

    /// Per-host timeout.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Export an environment variable for the command.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Working directory for the command (`cd` before running).
    pub fn dir(mut self, dir: impl Into<String>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// Feed `input` to the command's stdin.
    pub fn input(mut self, input: impl Into<String>) -> Self {
        self.input = Some(input.into());
        self
    }

    /// Stream stdout/stderr live: `cb(host, stream_type, chunk)` is called for
    /// every chunk as it arrives. The chunks are still accumulated into the
    /// final [`HostResult`] too.
    pub fn stream<F>(mut self, cb: F) -> Self
    where
        F: Fn(&str, StreamType, &[u8]) + Send + Sync + 'static,
    {
        self.stream = Some(Arc::new(cb));
        self
    }

    /// Resolve hosts and run, collecting per-host results.
    pub async fn run(self) -> Result<ExecResult> {
        let hosts = self.dispatch.inventory.resolve(&self.patterns)?;
        let script = build_script(&self.command, &self.env, self.dir.as_deref());
        let timeout = self.timeout;
        let input = self.input;
        let stream = self.stream;
        let conn = self.dispatch.conn.clone();

        let results = stream::iter(hosts)
            .map(|host| {
                let script = script.clone();
                let input = input.clone();
                let stream = stream.clone();
                let conn = conn.clone();
                async move {
                    let result =
                        run_one(&host, &conn, &script, timeout, input.as_deref(), stream).await;
                    (host, result)
                }
            })
            .buffer_unordered(self.parallel)
            .collect::<Vec<_>>()
            .await;

        let mut map = BTreeMap::new();
        for (host, result) in results {
            let hr = result.unwrap_or_else(|e| HostResult {
                host: host.clone(),
                success: false,
                exit_code: -1,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(e.to_string()),
            });
            map.insert(host, hr);
        }
        Ok(ExecResult { hosts: map })
    }
}

/// Wrap the user command with optional `cd` / env exports, run via `sh -c`.
fn build_script(cmd: &str, env: &BTreeMap<String, String>, dir: Option<&str>) -> String {
    let mut s = String::new();
    if let Some(d) = dir {
        s.push_str(&format!("cd {} && ", shell_quote(d)));
    }
    for (k, v) in env {
        s.push_str(&format!("export {}={}; ", k, shell_quote(v)));
    }
    s.push_str(cmd);
    s
}

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn run_one(
    host: &str,
    conn: &Conn,
    script: &str,
    timeout: Duration,
    input: Option<&str>,
    stream: Option<StreamCallback>,
) -> Result<HostResult> {
    match tokio::time::timeout(
        timeout,
        exec_via_ssh(host, conn, script, input, stream.as_ref()),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Ok(HostResult {
            host: host.to_string(),
            success: false,
            exit_code: -1,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("timed out after {timeout:?}")),
        }),
    }
}

/// Run a single command on a host over a fresh ssh session.
pub(crate) async fn exec_via_ssh(
    host: &str,
    conn: &Conn,
    script: &str,
    input: Option<&str>,
    stream: Option<&StreamCallback>,
) -> Result<HostResult> {
    let session = conn.connect(host).await?;
    let result = run_on_session(&session, conn, host, script, input, stream).await;
    let _ = session.close().await;
    result
}

/// Run a command on an already-open session (lets callers reuse one connection
/// for several commands, e.g. the `update` checksum/backup/chmod sequence).
pub(crate) async fn run_on_session(
    session: &openssh::Session,
    conn: &Conn,
    host: &str,
    script: &str,
    input: Option<&str>,
    stream: Option<&StreamCallback>,
) -> Result<HostResult> {
    use openssh::Stdio;

    let (program, args) = conn.shell_argv(script);
    let mut cmd = session.command(program);
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd.spawn().await?;
    let stdin = child.stdin().take();
    let stdout = child.stdout().take();
    let stderr = child.stderr().take();

    let mut out_acc: Vec<u8> = Vec::new();
    let mut err_acc: Vec<u8> = Vec::new();

    let write_stdin = async {
        if let (Some(mut si), Some(inp)) = (stdin, input) {
            use tokio::io::AsyncWriteExt;
            let _ = si.write_all(inp.as_bytes()).await;
            let _ = si.flush().await;
            // `si` dropped here -> EOF on the remote command's stdin
        }
    };
    let pump_out = pump(stdout, host, StreamType::Stdout, stream, &mut out_acc);
    let pump_err = pump(stderr, host, StreamType::Stderr, stream, &mut err_acc);

    tokio::join!(write_stdin, pump_out, pump_err);

    let status = child.wait().await?;

    Ok(HostResult {
        host: host.to_string(),
        success: status.success(),
        exit_code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out_acc).into_owned(),
        stderr: String::from_utf8_lossy(&err_acc).into_owned(),
        error: None,
    })
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
    host: &str,
    ty: StreamType,
    cb: Option<&StreamCallback>,
    acc: &mut Vec<u8>,
) {
    use tokio::io::AsyncReadExt;
    let Some(mut r) = reader else { return };
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Some(cb) = cb {
                    cb(host, ty, &buf[..n]);
                }
                acc.extend_from_slice(&buf[..n]);
            }
        }
    }
}
