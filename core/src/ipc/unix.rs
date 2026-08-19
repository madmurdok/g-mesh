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

impl Endpoint {
    /// The socket file for a project, as `daemon::endpoint` derives it.
    pub fn at_path(path: PathBuf) -> Self {
        Self(path)
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
