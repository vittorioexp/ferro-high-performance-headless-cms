#[derive(Debug, thiserror::Error)]
pub enum FerroError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Unauthorized(String),
    /// 401 with a specific code the client can act on (`OTP_REQUIRED`, `INVALID_OTP`).
    #[error("{1}")]
    AuthCode(&'static str, String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, FerroError>;

impl From<serde_json::Error> for FerroError {
    fn from(e: serde_json::Error) -> Self {
        FerroError::Invalid(e.to_string())
    }
}

impl From<std::io::Error> for FerroError {
    fn from(e: std::io::Error) -> Self {
        FerroError::Internal(e.to_string())
    }
}

impl FerroError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        FerroError::Invalid(msg.into())
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        FerroError::Forbidden(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        FerroError::NotFound(msg.into())
    }

    /// PostgreSQL SQLSTATE when the error comes from the database.
    pub fn sqlstate(&self) -> Option<String> {
        match self {
            FerroError::Db(sqlx::Error::Database(e)) => e.code().map(|c| c.to_string()),
            _ => None,
        }
    }
}
