//! Request/response control socket for the `handy` CLI.
//!
//! `tauri-plugin-single-instance` already forwards argv to a running instance,
//! but its callback returns `()` and the second process exits inside the
//! plugin's own setup hook — it is structurally one-way, so it can toggle
//! recording but can never hand a transcript back. This module is the channel
//! that can.
//!
//! Only the **server** lives here. The protocol, the endpoint layout, the
//! transport and the client all live in the `handy-core` crate, because the
//! CLI binary needs them and must not link Tauri or the inference stack to get
//! them. They are re-exported below so app-side code keeps one import path.
//!
//! Security: the socket can start a recording, inject keystrokes, and read
//! transcripts, so it lives in a `0700` per-user directory and the server
//! checks peer credentials. It deliberately never opens a caller-supplied path
//! by default — the client decodes audio and sends samples — so it is not a
//! file-read oracle.

pub mod handlers;
pub mod server;

pub use handy_core::{client, endpoint, protocol, transport};
pub use server::{shutdown, start, IpcServer};
