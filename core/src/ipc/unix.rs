//! The AF_UNIX half of [`crate::ipc`] - the transport this project has always
//! used, renamed rather than reimplemented.
//!
//! Every type here is a newtype (or a plain alias) over the `std`/`tokio`
//! type the daemon named directly before the port: `Stream` *is* a
//! `std::os::unix::net::UnixStream`, `Listener` *is* a
//! `std::os::unix::net::UnixListener`, [`AsyncStream`] is literally
//! `tokio::net::UnixStream`. The socket is still a file at
//! `~/.g-mesh/projects/<hash>/daemon.sock`, still unlinked before a bind and
//! still unlinked by `g-mesh stop`. Nothing about the Unix path's behaviour,
//! its syscalls or its cost changed; only who spells the type name did.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Where a project's daemon can be reached: on Unix, a filesystem path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(PathBuf);

/// How many bytes of path an AF_UNIX address can hold, including the trailing
/// NUL: 104 on macOS, 108 on Linux.
///
/// Derived from the struct rather than written down, because the two platforms
/// differ and a hardcoded 104 would silently under-report on Linux.
const SUN_PATH_CAPACITY: usize =
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);

/// The capacity is derived rather than written down, so this pins the shape it
/// must have: the two values the manual pages give (104 on macOS, 108 on
/// Linux) and nothing else. A compile-time assertion rather than a test,
/// because a platform where this lands somewhere else is one where the
/// derivation is wrong - that should fail the build, not one test case.
const _: () = assert!(SUN_PATH_CAPACITY == 104 || SUN_PATH_CAPACITY == 108);

impl Endpoint {
    /// The socket file for a project, as `daemon::endpoint` derives it.
    pub fn at_path(path: PathBuf) -> Self {
        Self(path)
    }

    /// Whether this path fits in an AF_UNIX address at all.
    ///
    /// Nothing checked this until a state root deep enough to overrun it was
    /// made reachable on purpose: `G_MESH_HOME` is a documented override, and
    /// pointing it somewhere nested produces a socket path over the limit.
    /// What the caller then saw was the shim's bootstrap timeout ten seconds
    /// later, carrying the OS's own `path must be shorter than SUN_LEN` and
    /// naming no fix - ten seconds per call, spent retrying a connect that
    /// could never succeed, against a daemon that had already died on bind.
    ///
    /// The default `~/.g-mesh` is nowhere near the limit, which is why this
    /// went unnoticed: only a deliberately relocated state root reaches it.
    pub fn check_length(&self) -> Result<(), String> {
        let len = self.0.as_os_str().len();
        // The stored path is NUL-terminated inside the address, so the usable
        // capacity is one byte short of the field.
        if len < SUN_PATH_CAPACITY {
            return Ok(());
        }
        Err(format!(
            "g-mesh: the socket path is {len} bytes, and this platform allows at most {} \n\
             \n  {}\n\n\
             A Unix domain socket address cannot hold a longer path, so no daemon can listen \
             here. This is reachable only with G_MESH_HOME pointing at a deep directory - set it \
             somewhere shorter (the default ~/.g-mesh is well inside the limit).",
            SUN_PATH_CAPACITY - 1,
            self.0.display(),
        ))
    }

    /// The socket file itself, for the callers that still have to treat it as
    /// a file (`cli::stop` clearing one, the tests waiting for one to appear).
    /// Has no counterpart on Windows, which is why nothing in the shared code
    /// paths calls it.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Removes a socket file left behind by a daemon that is already gone.
    ///
    /// An AF_UNIX socket outlives the process that bound it, and a leftover
    /// one makes `bind()` fail with `AddrInUse` forever - so this is what
    /// `daemon::run` calls before binding and what `cli::stop` calls once
    /// nothing is answering. Best-effort by design: a path that is not there
    /// is the outcome this asks for.
    ///
    /// The Windows implementation of this is a no-op, and that asymmetry is
    /// the whole of the "socket identity" difference - see
    /// [`crate::ipc::windows::Endpoint::clear_stale`].
    pub fn clear_stale(&self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

/// A blocking connection to a project's daemon.
#[derive(Debug)]
pub struct Stream(UnixStream);

impl Stream {
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        UnixStream::connect(&endpoint.0).map(Self)
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    /// Half-closes one direction, so a peer reading the other end sees EOF
    /// while replies already in flight can still arrive.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.shutdown(how)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A listener bound synchronously, before any tokio runtime exists.
#[derive(Debug)]
pub struct Listener(UnixListener);

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        // Checked before the syscall so the daemon's own stderr says what is
        // wrong and what to do, rather than leaving the OS's `SUN_LEN` string
        // as the only account of it.
        if let Err(message) = endpoint.check_length() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
        }
        UnixListener::bind(&endpoint.0).map(Self)
    }

    /// Hands the already-bound listener to tokio. Must be called from inside
    /// a runtime; `daemon::serve_forever` does it as the first thing in its
    /// `block_on`.
    pub fn into_async(self) -> io::Result<AsyncListener> {
        self.0.set_nonblocking(true)?;
        tokio::net::UnixListener::from_std(self.0).map(AsyncListener)
    }
}

/// The accept side, once tokio owns it.
#[derive(Debug)]
pub struct AsyncListener(tokio::net::UnixListener);

impl AsyncListener {
    pub async fn accept(&mut self) -> io::Result<AsyncStream> {
        self.0.accept().await.map(|(stream, _addr)| stream)
    }
}

/// One accepted connection, as `rmcp` consumes it (`AsyncRead + AsyncWrite`).
pub type AsyncStream = tokio::net::UnixStream;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_path_is_accepted() {
        assert!(Endpoint::at_path(PathBuf::from("/tmp/g-mesh/daemon.sock")).check_length().is_ok());
    }

    /// The boundary in both directions, because an off-by-one here fails in
    /// the expensive direction: an endpoint waved through is a daemon that
    /// dies on bind, ten seconds after the shim commits to it.
    #[test]
    fn the_limit_is_the_last_byte_that_leaves_room_for_the_nul() {
        let longest_usable = "/".repeat(SUN_PATH_CAPACITY - 1);
        assert_eq!(longest_usable.len(), SUN_PATH_CAPACITY - 1);
        assert!(Endpoint::at_path(PathBuf::from(&longest_usable)).check_length().is_ok());

        let one_too_long = "/".repeat(SUN_PATH_CAPACITY);
        assert!(Endpoint::at_path(PathBuf::from(one_too_long)).check_length().is_err());
    }

    /// The message is the whole point of the check - the OS already refuses
    /// the bind, it just refuses it uninformatively - so it must name the
    /// offending path, both numbers, and the override that is the only way
    /// to get here.
    #[test]
    fn the_message_says_what_to_do_about_it() {
        let path = format!("/{}/daemon.sock", "deep".repeat(40));
        let message = Endpoint::at_path(PathBuf::from(&path)).check_length().unwrap_err();
        assert!(message.contains(&path), "must name the path: {message}");
        assert!(message.contains(&path.len().to_string()), "must give the actual length: {message}");
        assert!(message.contains("G_MESH_HOME"), "must name the way out: {message}");
    }

    /// Bind refuses it too, and with the same message rather than the OS's -
    /// the daemon's stderr is the only account of the failure a log-reading
    /// caller ever gets.
    #[test]
    fn bind_refuses_an_over_long_path_before_the_syscall() {
        let endpoint = Endpoint::at_path(PathBuf::from(format!("/{}/daemon.sock", "deep".repeat(40))));
        let error = Listener::bind(&endpoint).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("G_MESH_HOME"), "{error}");
    }
}
