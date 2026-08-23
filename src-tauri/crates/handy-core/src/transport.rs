//! The only module that knows which local-socket implementation is in use.
//!
//! Everything above this speaks `impl Read + Write`, so swapping `interprocess`
//! for hand-rolled `UnixListener` + Win32 named pipes would not touch the
//! protocol, the server, or the client.

use crate::endpoint;
use anyhow::{Context, Result};
use interprocess::local_socket::traits::{Listener as _, Stream as _, StreamCommon as _};
use interprocess::local_socket::{ListenerOptions, Name, Stream};
use log::{debug, info, warn};
use std::io::{Read, Write};
use std::path::PathBuf;

/// A connected peer.
pub struct Connection {
    stream: Stream,
}

impl Connection {
    /// Split into independently owned halves.
    ///
    /// This is what lets the connection keep reading `Cancel` frames on one
    /// thread while a long-running job writes progress from another.
    pub fn split(self) -> (impl Read + Send, impl Write + Send) {
        let (rx, tx) = Stream::split(self.stream);
        (rx, tx)
    }

    /// Effective uid of the peer process, where the platform reports it.
    ///
    /// The `0700` endpoint directory is the primary gate; this is a second
    /// check in case it was somehow bypassed.
    pub fn peer_uid(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            self.stream.peer_creds().ok().and_then(|c| c.euid())
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}

pub struct Listener {
    inner: interprocess::local_socket::Listener,
}

impl Listener {
    pub fn accept(&self) -> Result<Connection> {
        let stream = self
            .inner
            .accept()
            .context("accepting a control connection")?;
        Ok(Connection { stream })
    }
}

fn name_for(target: &str) -> Result<Name<'_>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        target
            .to_ns_name::<GenericNamespaced>()
            .context("building the pipe name")
    }
    #[cfg(not(windows))]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        std::path::Path::new(target)
            .to_fs_name::<GenericFilePath>()
            .context("building the socket name")
    }
}

/// Bind the control socket, clearing a stale one left behind by a crash.
pub fn bind() -> Result<Listener> {
    endpoint::ensure_private_dir()?;
    let target = endpoint::transport_name()?;
    let path: Option<PathBuf> = if cfg!(windows) {
        None
    } else {
        Some(PathBuf::from(&target))
    };

    let listener = match try_bind(&target) {
        Ok(listener) => listener,
        Err(err) if is_addr_in_use(&err) => {
            // bind() reports AddrInUse for a stale socket file exactly as it
            // does for a live one, so probe before unlinking — otherwise a
            // second instance would happily steal a running instance's socket.
            //
            // Deliberately not using ListenerOptions::try_overwrite: it
            // displaces a *live* listener too, which is the failure mode this
            // check exists to prevent.
            if probe_is_live(&target) {
                anyhow::bail!("another Handy instance is already listening on {}", target);
            }
            warn!("Clearing a stale CLI control socket at {}", target);
            if let Some(path) = &path {
                let _ = std::fs::remove_file(path);
            }
            try_bind(&target)
                .with_context(|| format!("re-binding {} after clearing a stale socket", target))?
        }
        Err(err) => return Err(err).with_context(|| format!("binding {}", target)),
    };

    if let Some(path) = &path {
        endpoint::restrict_socket_file(path);
    }
    info!("CLI control socket listening on {}", target);
    Ok(Listener { inner: listener })
}

fn try_bind(target: &str) -> Result<interprocess::local_socket::Listener> {
    let name = name_for(target)?;
    // reclaim_name defaults to true, so the socket file is unlinked when the
    // listener drops; no manual cleanup here, which would risk deleting a
    // newly-bound socket from a restarted instance.
    ListenerOptions::new()
        .name(name)
        .create_sync()
        .map_err(anyhow::Error::from)
}

fn is_addr_in_use(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse)
}

/// Whether something actually answers on `target`.
fn probe_is_live(target: &str) -> bool {
    // A descriptor naming a dead pid is conclusive, and cheaper than connecting.
    if let Ok(Some(descriptor)) = endpoint::read_descriptor() {
        if descriptor.endpoint == target && !endpoint::process_is_alive(descriptor.pid) {
            debug!(
                "Descriptor names pid {} which is gone; treating the socket as stale",
                descriptor.pid
            );
            return false;
        }
    }
    match name_for(target) {
        Ok(name) => Stream::connect(name).is_ok(),
        Err(_) => false,
    }
}

/// Connect to a running instance's control socket.
pub fn connect() -> Result<Connection> {
    let target = endpoint::transport_name()?;

    // Refuse to talk through a directory another user could have created — it
    // would let them answer our requests and read the audio we send.
    #[cfg(unix)]
    if let Some(dir) = std::path::Path::new(&target).parent() {
        endpoint::verify_dir_ownership(dir)?;
    }

    let name = name_for(&target)?;
    let stream = Stream::connect(name).with_context(|| format!("connecting to {}", target))?;
    Ok(Connection { stream })
}
