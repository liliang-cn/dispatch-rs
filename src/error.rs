use thiserror::Error;

/// Errors returned by the library.
#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ssh error: {0}")]
    Ssh(#[from] openssh::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("no hosts matched the given patterns")]
    NoHosts,
}

pub type Result<T> = std::result::Result<T, Error>;
