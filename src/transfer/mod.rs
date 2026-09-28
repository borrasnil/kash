//! File transfer: upload and download via the remote shell.
//!
//! Streams base64-encoded data with SHA256 verification, progress reporting,
//! and platform-specific backends for Linux/macOS and Windows.

mod download;
mod protocol;
mod upload;
mod windows;

pub use download::download;
pub use upload::upload;
