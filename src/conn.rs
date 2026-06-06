//! Connection settings shared by every remote operation: how to open an ssh
//! session and how to wrap commands (sudo) for a given [`crate::Config`].

use crate::config::{Config, HostKeyChecking};
use crate::error::Result;
use openssh::{KnownHosts, Session, SessionBuilder};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub(crate) struct Conn {
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<PathBuf>,
    pub connect_timeout: Option<Duration>,
    pub sudo: bool,
    pub host_key_checking: HostKeyChecking,
    pub known_hosts_file: Option<PathBuf>,
}

impl Conn {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            user: cfg.user.clone(),
            port: cfg.port,
            identity: cfg.identity.clone(),
            connect_timeout: cfg.connect_timeout,
            sudo: cfg.sudo,
            host_key_checking: cfg.host_key_checking,
            known_hosts_file: cfg.known_hosts_file.clone(),
        }
    }

    /// Open a session to `host` (which may be `user@host`) applying the config.
    pub async fn connect(&self, host: &str) -> Result<Session> {
        let mut b = SessionBuilder::default();
        b.known_hosts_check(match self.host_key_checking {
            HostKeyChecking::Strict => KnownHosts::Strict,
            HostKeyChecking::AcceptNew => KnownHosts::Add,
            HostKeyChecking::AcceptAny => KnownHosts::Accept,
        });
        if let Some(f) = &self.known_hosts_file {
            b.user_known_hosts_file(f);
        }
        if let Some(u) = &self.user {
            b.user(u.clone());
        }
        if let Some(p) = self.port {
            b.port(p);
        }
        if let Some(k) = &self.identity {
            b.keyfile(k);
        }
        if let Some(t) = self.connect_timeout {
            b.connect_timeout(t);
        }
        Ok(b.connect(host).await?)
    }

    /// Program + args to run a shell `script`, honoring sudo.
    pub fn shell_argv(&self, script: &str) -> (&'static str, Vec<String>) {
        if self.sudo {
            (
                "sudo",
                vec!["-n".into(), "sh".into(), "-c".into(), script.into()],
            )
        } else {
            ("sh", vec!["-c".into(), script.into()])
        }
    }
}
