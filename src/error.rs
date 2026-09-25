use std::fmt;

use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Unauthorized,
    VaultDenied {
        vault_id: Uuid,
    },
    NotFound(&'static str),
    Invalid(String),
    Sqlite(rusqlite::Error),
    Yaml(serde_yaml::Error),
    Json(serde_json::Error),
    Io(std::io::Error),
    /// `source.lattice_path` does not exist. Fail closed; do not create it.
    SourceMissing(String),
    /// Lattice tables drifted from the columns the adapter reads.
    SourceSchemaDrift(String),
    /// `SQLITE_BUSY` / `SQLITE_LOCKED` after the busy timeout.
    SourceLocked(String),
    /// Unreadable file, or a WAL `-shm` that cannot be opened.
    SourceUnreadable(String),
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
            Error::SourceMissing(path) => write!(f, "lattice source missing: {path}"),
            Error::SourceSchemaDrift(detail) => write!(f, "lattice schema drift: {detail}"),
            Error::SourceLocked(detail) => write!(f, "lattice source locked: {detail}"),
            Error::SourceUnreadable(detail) => write!(f, "lattice source unreadable: {detail}"),
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
