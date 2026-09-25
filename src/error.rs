//! Typed errors for the kash crate.
//!
//! Uses [`thiserror`] to define fallible operations as an enum.
//! The three variants cover:
//!
//! * [`BindFailed`](ShellHandlerError::BindFailed) — binding to the
//!   listen address/port failed.
//! * [`AcceptFailed`](ShellHandlerError::AcceptFailed) — accepting an
//!   incoming reverse-shell connection failed.
//! * [`Io`](ShellHandlerError::Io) — general I/O errors (auto-converted
//!   via `?`).
//!
//! Binary-level error reporting is left to [`anyhow`], which provides
//! human-readable backtraces via the `{e:#}` format specifier in
//! [`main`](crate::main).

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ShellHandlerError {
    #[error("Failed to bind to {addr}: {source}")]
    BindFailed {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to accept connection on {addr}: {source}")]
    AcceptFailed {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
