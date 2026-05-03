//! Crate-owned errors. Mapped to `ApiError` only in `routes.rs`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BaitongError {
    #[error("baitong adapter is not reachable: {0}")]
    Unreachable(String),

    #[error("baitong adapter rejected the credentials")]
    InvalidCredentials,

    #[error("baitong credentials are missing or cannot be decrypted: {0}")]
    CredentialsRequired(String),

    #[error("baitong adapter failed: {0}")]
    Adapter(String),

    #[error("configuration was changed by another save (expected {expected}, current {current})")]
    ConfigConflict { expected: u64, current: u64 },

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl BaitongError {
    /// Stable machine code for the HTTP boundary.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreachable(_) => "BAITONG_NETWORK_ERROR",
            Self::InvalidCredentials => "BAITONG_INVALID_CREDENTIALS",
            Self::CredentialsRequired(_) => "BAITONG_CREDENTIALS_REQUIRED",
            Self::Adapter(_) => "BAITONG_ADAPTER_ERROR",
            Self::ConfigConflict { .. } => "BAITONG_CONFIG_CONFLICT",
            Self::BadRequest(_) => "BAITONG_BAD_REQUEST",
            Self::Internal(_) => "BAITONG_INTERNAL",
        }
    }
}
