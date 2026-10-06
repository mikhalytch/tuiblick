use thiserror::Error;

/// Application-wide error type (spec §8).
///
/// `anyhow` is only used at the `main` boundary; all modules report
/// failures through this enum.
///
/// Task 1 scaffold: no module constructs these variants yet (first
/// consumer lands in Task 2 `config`), so dead-code is allowed here.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("sudo authentication failed")]
    SudoAuth,

    #[error("command `{cmd}` exited with {code:?}: {tail}")]
    Process {
        cmd: String,
        code: Option<i32>,
        tail: String,
    },

    #[error("network probe failed: {0}")]
    NetProbe(String),

    #[error("{0}")]
    User(String),
}
