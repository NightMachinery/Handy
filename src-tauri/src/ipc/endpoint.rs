//! Where the control socket lives, and how client and server agree on it.
//!
//! Both sides compute the path with the same function, so they cannot drift.
//! The socket sits inside a `0700` directory owned by the current user: the
//! *directory* is the authorization boundary, which is why there is no token.
//!
//! Deliberately not reusing `tauri-plugin-single-instance`'s
//! `/tmp/com_pais_handy_si.sock` — that lives directly in a world-writable
//! sticky directory, where another local user can pre-create the path and
//! intercept connections.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Override the endpoint directory suffix, so a dev build and an installed app
/// can coexist. Read identically by client and server.
pub const IPC_NAME_ENV: &str = "HANDY_IPC_NAME";

/// `sun_path` is 104 bytes on macOS and 108 on Linux. Stay well clear.
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Written next to the socket so a client can report a useful error before
/// connecting, and can tell a crashed instance from a live one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    /// Version of this file's own shape.
    pub schema: u32,
    /// Wire protocol the running app speaks.
    pub protocol: u32,
    /// Oldest protocol the running app accepts.
    pub min_protocol: u32,
    pub app_version: String,
    pub pid: u32,
    /// Socket path on Unix, pipe name on Windows.
    pub endpoint: String,
}

pub const DESCRIPTOR_SCHEMA: u32 = 1;

/// Directory holding the socket and descriptor, created `0700` on demand.
pub fn endpoint_dir() -> Result<PathBuf> {
    let base = base_dir();
    let mut name = format!("handy-{}", user_id());
    if let Some(suffix) = std::env::var_os(IPC_NAME_ENV) {
        let suffix = suffix.to_string_lossy();
        if !suffix.is_empty() {
            name.push('-');
            name.push_str(sanitize(&suffix).as_str());
        }
    }
    Ok(base.join(name))
}

/// Path of the control socket (or, on Windows, of the descriptor's directory).
pub fn socket_path() -> Result<PathBuf> {
    let candidate = endpoint_dir()?.join("ipc.sock");
    if candidate.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        // A deeply nested $TMPDIR would otherwise produce a path that bind()
        // silently truncates.
        let fallback = PathBuf::from("/tmp")
            .join(format!("handy-{}", user_id()))
            .join("ipc.sock");
        return Ok(fallback);
    }
    Ok(candidate)
}

pub fn descriptor_path() -> Result<PathBuf> {
    Ok(endpoint_dir()?.join("ipc.json"))
}

/// Name the transport should bind/connect to.
///
/// Unix uses the filesystem path; Windows uses a named pipe, which has no
/// directory to protect, so the name carries the user id instead.
pub fn transport_name() -> Result<String> {
    #[cfg(windows)]
    {
        let mut name = format!(r"\\.\pipe\handy-ipc-{}", user_id());
        if let Some(suffix) = std::env::var_os(IPC_NAME_ENV) {
            let suffix = suffix.to_string_lossy();
            if !suffix.is_empty() {
                name.push('-');
                name.push_str(sanitize(&suffix).as_str());
            }
        }
        Ok(name)
    }
    #[cfg(not(windows))]
    {
        Ok(socket_path()?.to_string_lossy().into_owned())
    }
}

/// Create the endpoint directory with owner-only permissions.
pub fn ensure_private_dir() -> Result<PathBuf> {
    let dir = socket_path()?
        .parent()
        .map(Path::to_path_buf)
        .context("endpoint path has no parent directory")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !dir.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        } else {
            // Tighten an existing directory rather than trusting it.
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    Ok(dir)
}

/// Refuse to talk through a directory somebody else can write to.
///
/// Cheap defense against a hijacked `$TMPDIR`: without it, another local user
/// who can create the directory first could stand up a socket that answers our
/// requests, and would see the audio we send.
#[cfg(unix)]
pub fn verify_dir_ownership(dir: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let meta = fs::metadata(dir).with_context(|| format!("stat {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if meta.uid() != user_id() {
        bail!(
            "{} is owned by uid {}, not by you (uid {})",
            dir.display(),
            meta.uid(),
            user_id()
        );
    }
    let mode = meta.permissions().mode() & 0o077;
    if mode != 0 {
        bail!(
            "{} is accessible to other users (mode {:o})",
            dir.display(),
            meta.permissions().mode() & 0o777
        );
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn verify_dir_ownership(_dir: &Path) -> Result<()> {
    // Named pipes carry the creating token's default DACL; there is no
    // directory to check.
    Ok(())
}

/// Restrict the socket file itself. Belt and braces — the directory is the real
/// gate, and some platforms ignore socket mode bits entirely.
#[cfg(unix)]
pub fn restrict_socket_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn restrict_socket_file(_path: &Path) {}

pub fn write_descriptor(descriptor: &Descriptor) -> Result<()> {
    let path = descriptor_path()?;
    let json = serde_json::to_vec_pretty(descriptor)?;
    fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn read_descriptor() -> Result<Option<Descriptor>> {
    let path = descriptor_path()?;
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Remove the socket and descriptor. Best effort — called on shutdown.
pub fn cleanup() {
    if let Ok(path) = socket_path() {
        let _ = fs::remove_file(path);
    }
    if let Ok(path) = descriptor_path() {
        let _ = fs::remove_file(path);
    }
}

/// Whether a process with this pid still exists.
pub fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signal 0 performs error checking without delivering a signal.
        // EPERM means it exists but belongs to someone else, which still
        // counts as alive.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        // Windows pipes vanish with their process, so staleness never arises.
        false
    }
}

fn base_dir() -> PathBuf {
    // XDG_RUNTIME_DIR is already 0700, on tmpfs, and cleaned at logout.
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return path;
        }
    }
    // macOS gives each user a private per-boot $TMPDIR under /var/folders.
    if let Some(dir) = std::env::var_os("TMPDIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return path;
        }
    }
    PathBuf::from("/tmp")
}

fn user_id() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        // No uid on Windows; the username keeps two logged-in users apart.
        use std::hash::{Hash, Hasher};
        let name = std::env::var("USERNAME").unwrap_or_else(|_| "handy".into());
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        hasher.finish() as u32
    }
}

/// Keep an env-supplied suffix from escaping the directory or breaking a name.
fn sanitize(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests mutate process-wide environment variables, so they must not
    /// interleave. Rust runs tests in threads within one process, hence the lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let previous = std::env::var_os(key);
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn name_env_suffixes_the_directory_for_both_sides() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set(IPC_NAME_ENV, Some("devbuild"));
        let dir = endpoint_dir().unwrap();
        assert!(
            dir.to_string_lossy().ends_with("-devbuild"),
            "got {}",
            dir.display()
        );
    }

    #[test]
    fn name_env_cannot_escape_the_base_directory() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::set(IPC_NAME_ENV, Some("../../etc/passwd"));
        let dir = endpoint_dir().unwrap();
        let text = dir.to_string_lossy();
        assert!(!text.contains(".."), "path traversal not stripped: {text}");
        assert!(
            !text.contains("/etc/"),
            "path traversal not stripped: {text}"
        );
    }

    #[test]
    fn empty_name_env_is_ignored() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let plain = {
            let _guard = EnvGuard::set(IPC_NAME_ENV, None);
            endpoint_dir().unwrap()
        };
        let _guard = EnvGuard::set(IPC_NAME_ENV, Some(""));
        assert_eq!(endpoint_dir().unwrap(), plain);
    }

    #[test]
    fn a_long_tmpdir_falls_back_to_a_short_path() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _name = EnvGuard::set(IPC_NAME_ENV, None);
        // Must exist to be accepted as a base, so build it under the real temp dir.
        let deep = std::env::temp_dir().join("a".repeat(120));
        std::fs::create_dir_all(&deep).unwrap();
        let _guard = EnvGuard::set("TMPDIR", Some(deep.to_str().unwrap()));

        let path = socket_path().unwrap();
        assert!(
            path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
            "sun_path guard did not trip: {} bytes",
            path.as_os_str().len()
        );
        assert!(path.starts_with("/tmp"), "got {}", path.display());
        let _ = std::fs::remove_dir_all(&deep);
    }

    #[test]
    fn xdg_runtime_dir_wins_over_tmpdir() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _name = EnvGuard::set(IPC_NAME_ENV, None);
        let xdg = std::env::temp_dir().join("handy-xdg-test");
        std::fs::create_dir_all(&xdg).unwrap();
        let _guard = EnvGuard::set("XDG_RUNTIME_DIR", Some(xdg.to_str().unwrap()));
        assert!(endpoint_dir().unwrap().starts_with(&xdg));
        let _ = std::fs::remove_dir_all(&xdg);
    }

    #[test]
    fn a_nonexistent_base_is_skipped() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _name = EnvGuard::set(IPC_NAME_ENV, None);
        let _xdg = EnvGuard::set("XDG_RUNTIME_DIR", Some("/definitely/not/here"));
        let _tmp = EnvGuard::set("TMPDIR", None);
        assert!(endpoint_dir().unwrap().starts_with("/tmp"));
    }

    #[test]
    fn our_own_pid_is_alive_and_a_bogus_one_is_not() {
        assert!(process_is_alive(std::process::id()));
        // Well above any plausible live pid on macOS/Linux.
        assert!(!process_is_alive(0x7FFF_FFF0));
    }

    #[test]
    fn descriptor_round_trips() {
        let descriptor = Descriptor {
            schema: DESCRIPTOR_SCHEMA,
            protocol: 1,
            min_protocol: 1,
            app_version: "0.9.4".into(),
            pid: 1234,
            endpoint: "/tmp/handy-501/ipc.sock".into(),
        };
        let json = serde_json::to_vec(&descriptor).unwrap();
        let back: Descriptor = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.pid, 1234);
        assert_eq!(back.endpoint, descriptor.endpoint);
    }
}
