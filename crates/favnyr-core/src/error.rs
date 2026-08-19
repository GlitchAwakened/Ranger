use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid config: {0}")]
    Config(String),

    #[error("workspace: {0}")]
    Workspace(String),

    #[error("favorites: {0}")]
    Favorites(String),

    #[error("openers: {0}")]
    Openers(String),
}

pub type Result<T> = std::result::Result<T, Error>;
