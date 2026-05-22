//! Top-level error type. Most plugins return `Result<(), Error>`; the
//! executor aggregates a vector of these so a single bad action doesn't
//! abort the whole stage (mirrors Go's `multierror` usage).

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error at {path:?}: {source}")]
    Io {
        path: Option<PathBuf>,
        #[source]
        source: std::io::Error,
    },

    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("command `{cmd}` failed: exit {status:?}: {stderr}")]
    Cmd {
        cmd: String,
        status: Option<i32>,
        stderr: String,
        stdout: String,
    },

    #[error("template error: {0}")]
    Template(String),

    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("schema error: {0}")]
    Schema(String),

    #[error("plugin `{plugin}` failed: {source}")]
    Plugin {
        plugin: String,
        #[source]
        source: Box<Error>,
    },

    #[error("aggregate error ({} errors)", .0.len())]
    Multi(Vec<Error>),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(err: std::io::Error) -> Self {
        Self::Io { path: None, source: err }
    }
    pub fn io_at<P: Into<PathBuf>>(path: P, err: std::io::Error) -> Self {
        Self::Io { path: Some(path.into()), source: err }
    }
    pub fn other<S: Into<String>>(msg: S) -> Self {
        Self::Other(msg.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::io(e)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
