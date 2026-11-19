//! The one error this crate returns of its own; everything else here reports
//! problems through [`crate::diagnostics`] or the error types of the crates
//! it wraps.

/// Reading, writing or renaming this machine's identity.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("cannot resolve config directory")]
    NoConfigDir,
    #[error("{0}")]
    Name(String),
}
