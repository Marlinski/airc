//! SASL error type.

use thiserror::Error;

/// Errors that can occur during a SASL exchange.
#[derive(Debug, Error)]
pub enum SaslError {
    /// The client sent a malformed message (bad base64, missing fields, etc.).
    #[error("malformed message: {0}")]
    Malformed(String),

    /// Authentication credentials did not match.
    #[error("authentication failed")]
    AuthFailed,

    /// The mechanism received a message it did not expect at this stage.
    #[error("unexpected message")]
    UnexpectedMessage,
}
