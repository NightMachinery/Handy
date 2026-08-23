//! Request/response control socket for the `handy` CLI.
//!
//! `tauri-plugin-single-instance` already forwards argv to a running instance,
//! but its callback returns `()` and the second process exits inside the
//! plugin's own setup hook — it is structurally one-way, so it can toggle
//! recording but can never hand a transcript back. This module is the channel
//! that can.
//!
//! Layout:
//!
//! - [`protocol`] — frames and codec. No Tauri, so it unit-tests without an app.
//! - [`endpoint`] — where the socket lives; shared verbatim by both sides.
//! - [`transport`] — the only module that names a socket implementation.
//! - [`server`] — accept loop, job queue, cancellation.
//! - [`client`] — the blocking client used by the CLI.
//! - [`handlers`] — one module per request kind.
//!
//! Security: the socket can start a recording, inject keystrokes, and read
//! transcripts, so it lives in a `0700` per-user directory and the server
//! checks peer credentials. It deliberately never opens a caller-supplied path
//! by default — the client decodes audio and sends samples — so it is not a
//! file-read oracle.

pub mod client;
pub mod endpoint;
pub mod handlers;
pub mod protocol;
pub mod server;
pub mod transport;

pub use server::{shutdown, start, IpcServer};
