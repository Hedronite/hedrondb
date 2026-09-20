use std::fmt;

use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Unauthorized,
    VaultDenied { vault_id: Uuid },
    NotFound(&'static str),
    Invalid(String),
    Sqlite(rusqlite::Error),
    Yaml(serde_yaml::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unauthorized => write!(f, "agent token required or invalid"),
            Error::VaultDenied { vault_id } => {
                write!(f, "vault {vault_id} is isolated from this agent")
            }
            Error::NotFound(kind) => write!(f, "{kind} not found"),
            Error::Invalid(msg) => write!(f, "{msg}"),
            Error::Sqlite(err) => write!(f, "sqlite: {err}"),
            Error::Yaml(err) => write!(f, "yaml: {err}"),
            Error::Json(err) => write!(f, "json: {err}"),
            Error::Io(err) => write!(f, "io: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Error::Sqlite(err)
    }
}

impl From<serde_yaml::Error> for Error {
    fn from(err: serde_yaml::Error) -> Self {
        Error::Yaml(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}
