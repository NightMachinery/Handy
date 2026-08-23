//! Request handlers, one module per request kind.
//!
//! Adding a verb is four edits: a [`ClientFrame`](super::protocol::ClientFrame)
//! variant, a [`ResultBody`](super::protocol::ResultBody) variant, a module
//! here, and an arm in the server's dispatch. Framing, the handshake,
//! cancellation, heartbeats, and error mapping are all inherited.

pub mod status;
pub mod transcribe;
