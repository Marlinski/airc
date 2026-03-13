//! Client error types.

use thiserror::Error;

/// Errors that can occur during IRC client operations.
#[derive(Debug, Error)]
pub enum ClientError {
    /// TCP connection failed.
    #[error("connection failed: {0}")]
    Connect(#[from] std::io::Error),

    /// IRC registration was rejected by the server.
    #[error("registration failed: {0}")]
    Registration(String),

    /// The nickname is already in use.
    #[error("nickname in use: {0}")]
    NickInUse(String),

    /// The client is not connected.
    #[error("not connected")]
    NotConnected,

    /// The client is already connected.
    #[error("already connected")]
    AlreadyConnected,

    /// Send failed (connection closed).
    #[error("send failed: connection closed")]
    SendFailed,

    /// The operation timed out.
    #[error("operation timed out")]
    Timeout,

    /// Protocol parse error.
    #[error("protocol error: {0}")]
    Protocol(#[from] airc_shared::ParseError),
}
