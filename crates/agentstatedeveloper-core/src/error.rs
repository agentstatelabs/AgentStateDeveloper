use thiserror::Error;

#[derive(Debug, Error)]
pub enum AsdError {
    #[error("repository error: {0}")]
    Repo(#[from] agentstategraph::RepoError),

    #[error("storage error: {0}")]
    Storage(#[from] agentstategraph_storage::StorageError),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("language not registered: {0}")]
    UnknownLanguage(String),

    #[error("parse error: {0}")]
    Parse(String),

    #[error("symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("policy denied: {0}")]
    PolicyDenied(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AsdError>;
