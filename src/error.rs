use std::env::VarError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("missing environment variable: {0}")]
    MissingEnvVar(&'static str),

    #[error("invalid environment variable {name}: {source}")]
    InvalidEnvVar {
        name: &'static str,
        source: VarError,
    },

    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("API error: {status} - {message}")]
    Api { status: u16, message: String },

    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    #[error("validation failed: {}", .0.join(", "))]
    ValidationFailed(Vec<String>),
}

pub type Result<T> = std::result::Result<T, Error>;
