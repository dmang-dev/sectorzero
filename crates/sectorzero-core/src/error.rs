use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found in image: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    Exists(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("not a readable FAT volume: {0}")]
    NotFat(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Attach the image path to an I/O error from fatfs where the kind tells us something useful.
    pub(crate) fn at(path: &str, err: io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::NotFound => Error::NotFound(display(path)),
            io::ErrorKind::AlreadyExists => Error::Exists(display(path)),
            _ => Error::Io(err),
        }
    }
}

pub(crate) fn display(path: &str) -> String {
    format!("/{path}")
}
