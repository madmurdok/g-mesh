//! The named-pipe half of [`crate::ipc`].
//!
//! # Socket identity, which is the one thing that is not a translation
//!
//! On Unix a project's daemon is reachable at a *file*:
//! `~/.g-mesh/projects/<hash>/daemon.sock`. The directory carries the project
//! identity, the filesystem carries the name, and a crashed daemon leaves the
//! file behind - which is why `daemon::run` unlinks it before binding and why
//! `cli::stop` unlinks it on the way out.
//!
//! A Windows named pipe has none of that. Names live in one flat, machine-wide
//! `\\.\pipe\` namespace with no filesystem presence and no directory to hang
//! identity off, so:
//!
//! - **The name has to carry the project hash itself.** [`Endpoint::named`]
//!   builds `\\.\pipe\g-mesh-<hash>` from the same
//!   `daemon::identity::project_hash` the state directory is named after, so
//!   the shim and the daemon still derive one identity from the project root
//!   and nothing else. The pid, lock and build-stamp files stay exactly where
//!   they are: they are ordinary files and need no porting.
//! - **There is nothing to unlink, ever.** A pipe name exists for exactly as
//!   long as some process holds an instance of it open, and the kernel
//!   reclaims it when the last handle closes - including when the process is
//!   killed. [`Endpoint::clear_stale`] is therefore a deliberate no-op, and
//!   "a stale socket left by a crashed daemon", the whole reason the Unix
//!   path unlinks, is a state that cannot occur here.
//! - **Staleness shows up at bind time instead.** `FILE_FLAG_FIRST_PIPE_INSTANCE`
//!   makes [`Listener::bind`] fail with `ERROR_ACCESS_DENIED` when a live
//!   server already holds the name - the exact analogue of `AddrInUse`, and
//!   the reason it is safe to keep: `daemon::run` only reaches the bind
//!   holding the project's singleton lock, so a name that *is* taken means a
//!   live daemon this process must not displace, never a leftover.
//! - **The namespace is global, not per-user.** Two users on one machine
//!   share `\\.\pipe\`, and two projects that hash the same would collide -
//!   but they would already share `~/.g-mesh/projects/<hash>/` on Unix, so
//!   the collision domain is unchanged. What *is* new is that another user's
//!   daemon for the same path could hold the name; the pipe is created with
//!   the default security descriptor (owner + SYSTEM), so such a client is
//!   refused rather than served.
//!
//! # Why the blocking side is overlapped I/O rather than `std::fs::File`
//!
//! `File::open(r"\\.\pipe\...")` connects to a named pipe and gives `Read +
//! Write` for free, which is tempting. It is also wrong for this transport:
//! `shim::proxy` reads on one thread while another writes, and Windows
//! serializes concurrent operations on a *synchronous* handle - a blocked
//! read would hold the writer behind it, deadlocking the proxy the first time
//! the client went quiet. Opening with `FILE_FLAG_OVERLAPPED` and waiting on
//! a per-`Stream` event lets the two directions run at once, which is what
//! `UnixStream` gives on the other platform for nothing.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::NamedPipeServer;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, ERROR_SEM_TIMEOUT,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// In and out buffer size for each pipe instance. The kernel treats this as a
/// hint for how much it may buffer before a writer has to wait for a reader;
/// 64 KiB comfortably holds any single MCP frame this transport carries, so
/// an ordinary request/response never blocks on the pipe at all.
const PIPE_BUFFER: u32 = 64 * 1024;

/// How long [`Stream::connect`] keeps retrying `ERROR_PIPE_BUSY` before
/// giving up.
///
/// Busy means every existing instance is already connected and the server has
/// not yet created the next one - a window of microseconds in
/// [`AsyncListener::accept`], which creates the replacement instance
/// immediately after handing one off. Bounded rather than infinite because
/// this is also the code path `daemon::is_listening` runs, and that must
/// answer promptly.
const CONNECT_BUSY_BUDGET: Duration = Duration::from_secs(1);
const CONNECT_BUSY_WAIT_MS: u32 = 100;

/// Where a project's daemon can be reached: on Windows, a name in the
/// machine-wide pipe namespace. See this module's header for why that is a
/// different kind of thing from a socket file, and what follows from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(OsString);

impl Endpoint {
    /// `\\.\pipe\g-mesh-<id>`, where `id` is the project hash the state
    /// directory is named after (`daemon::endpoint` passes it in).
    pub fn named(id: &str) -> Self {
        Self(OsString::from(format!(r"\\.\pipe\g-mesh-{id}")))
    }

    /// Always reachable: a pipe name is built from the project hash, so it is
    /// a fixed short length no matter how deep the state directory sits. The
    /// Unix counterpart has a real limit to check (`SUN_PATH_CAPACITY`); this
    /// is a no-op for the same reason [`Self::unlink_if_stale`] is, so the
    /// shim can check unconditionally rather than branch on the platform.
    pub fn check_length(&self) -> Result<(), String> {
        Ok(())
    }

    /// Nothing to unlink: a pipe name lives exactly as long as an open handle
    /// to it, so there is no such thing as one left behind by a dead daemon.
    /// Kept as a no-op so the callers that have to clear a Unix socket file
    /// (`daemon::run` before its bind, `cli::stop` after the daemon is gone)
    /// stay platform-agnostic.
    pub fn clear_stale(&self) {}

    fn wide(&self) -> Vec<u16> {
        self.0.encode_wide().chain(std::iter::once(0)).collect()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.to_string_lossy())
    }
}

/// The pipe handle, shared by every [`Stream`] clone of one connection.
///
/// Shared rather than duplicated per clone because [`Stream::shutdown`] has to
/// reach a read that another thread is blocked in, and `CancelIoEx` can only
/// do that for operations issued against *this* handle.
#[derive(Debug)]
struct Connection {
    handle: OwnedHandle,
    shut_down: AtomicBool,
}

impl Connection {
    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }
}

/// A blocking connection to a project's daemon.
///
/// Cloning with [`Stream::try_clone`] shares the pipe handle and takes a
/// fresh completion event, so one clone may read while another writes - the
/// arrangement `shim::proxy` relies on.
#[derive(Debug)]
pub struct Stream {
    connection: Arc<Connection>,
    /// Manual-reset event this `Stream`'s own overlapped operations wait on.
    /// One per `Stream` rather than one per connection: two clones must not
    /// share it, or a read and a write would consume each other's completion.
    event: OwnedHandle,
}

impl Stream {
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        let name = endpoint.wide();
        let deadline = Instant::now() + CONNECT_BUSY_BUDGET;
        loop {
            // SAFETY: `name` is a NUL-terminated wide string that outlives the
            // call, and no other pointer argument is passed.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                // SAFETY: `CreateFileW` returned a fresh handle this process
                // owns and has not handed to anything else.
                return Self::over(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
            }

            // SAFETY: reads this thread's last error; no arguments.
            let code = unsafe { GetLastError() };
            if code != ERROR_PIPE_BUSY || Instant::now() >= deadline {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            // SAFETY: same NUL-terminated wide string as above.
            if unsafe { WaitNamedPipeW(name.as_ptr(), CONNECT_BUSY_WAIT_MS) } == 0 {
                // SAFETY: reads this thread's last error; no arguments.
                let code = unsafe { GetLastError() };
                if code != ERROR_SEM_TIMEOUT {
                    return Err(io::Error::from_raw_os_error(code as i32));
                }
            }
        }
    }

    fn over(handle: OwnedHandle) -> io::Result<Self> {
        Ok(Self {
            connection: Arc::new(Connection { handle, shut_down: AtomicBool::new(false) }),
            event: completion_event()?,
        })
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self { connection: Arc::clone(&self.connection), event: completion_event()? })
    }

    /// The nearest honest equivalent of a socket half-close, which named pipes
    /// do not have.
    ///
    /// A pipe has one direction-less connection, so "stop sending, keep
    /// receiving" cannot be expressed: the peer learns the request stream
    /// ended only when the whole connection does. This therefore cancels every
    /// operation outstanding on the handle (which is how a read blocked on
    /// another thread finds out) and marks the connection finished, so the
    /// proxy unwinds and the process exit that follows closes the handle - at
    /// which point the daemon sees EOF and ends the session.
    ///
    /// The cost, relative to Unix, is real and worth naming: a reply the
    /// daemon had not yet written when its client closed stdin is lost here,
    /// where `shutdown(Shutdown::Write)` on a `UnixStream` would still have
    /// delivered it. `how` is ignored for the same reason - there is only one
    /// thing this can do.
    pub fn shutdown(&self, _how: Shutdown) -> io::Result<()> {
        if self.connection.shut_down.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // SAFETY: a live handle this `Stream` owns a share of; a null
        // `lpOverlapped` asks for every operation on it to be cancelled,
        // whichever thread issued it.
        unsafe { CancelIoEx(self.connection.raw(), ptr::null()) };
        Ok(())
    }

    /// Issues one overlapped read or write and blocks until it completes.
    ///
    /// # Safety
    ///
    /// `buf`/`len` must describe a valid region for the direction implied by
    /// `write`, and must stay valid for the whole call - which they do,
    /// because the wait below is synchronous.
    unsafe fn transfer(&self, buf: *mut u8, len: usize, write: bool) -> io::Result<usize> {
        if self.connection.shut_down.load(Ordering::Acquire) {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }

        let handle = self.connection.raw();
        let len = u32::try_from(len).unwrap_or(u32::MAX);
        let mut overlapped: OVERLAPPED = std::mem::zeroed();
        overlapped.hEvent = self.event.as_raw_handle() as HANDLE;

        let started = if write {
            WriteFile(handle, buf as *const u8, len, ptr::null_mut(), &mut overlapped)
        } else {
            ReadFile(handle, buf, len, ptr::null_mut(), &mut overlapped)
        };
        if started == 0 {
            let code = GetLastError();
            if code != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }

        let mut transferred: u32 = 0;
        if GetOverlappedResult(handle, &overlapped, &mut transferred, 1) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        Ok(transferred as usize)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is a valid mutable region for `buf.len()` bytes and
        // outlives the synchronous call.
        match unsafe { self.transfer(buf.as_mut_ptr(), buf.len(), false) } {
            Ok(read) => Ok(read),
            // The peer closing its end of a pipe is an error code here and a
            // zero-length read on a socket. Reported as EOF, because that is
            // what every caller of this transport means by "the daemon hung
            // up", and what the Unix implementation gives them.
            //
            // `ERROR_OPERATION_ABORTED` joins them: it is what a read blocked
            // on this thread returns once `shutdown` has cancelled it, and
            // that is this side deciding the conversation is over rather than
            // a failure to report.
            Err(err) => match err.raw_os_error().map(|code| code as u32) {
                Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
                | Some(ERROR_OPERATION_ABORTED) => Ok(0),
                _ if err.kind() == io::ErrorKind::BrokenPipe => Ok(0),
                _ => Err(err),
            },
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` is a valid region for `buf.len()` bytes and outlives
        // the synchronous call; `transfer` only reads through it when `write`.
        unsafe { self.transfer(buf.as_ptr() as *mut u8, buf.len(), true) }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Deliberately not `FlushFileBuffers`: on a pipe that blocks until the
        // *peer* has drained everything sent, which would let a slow daemon
        // stall the shim's write pump. A completed overlapped write has
        // already reached the pipe's buffer, which is as far as the Unix
        // implementation's flush gets too.
        Ok(())
    }
}

/// A listener bound synchronously, before any tokio runtime exists.
///
/// This is the piece that made a hand-rolled implementation necessary rather
/// than merely preferable: `daemon::run` creates the pipe, and only then
/// writes the pid file and hands the accept side to tokio, so "the pid file
/// exists" keeps meaning "a client can connect". `CreateNamedPipeW` needs no
/// reactor, and [`Listener::into_async`] adopts the resulting handle exactly
/// the way `tokio::net::UnixListener::from_std` adopts a bound socket.
///
/// A client that connects in the gap between the two - after the bind, before
/// the accept loop is up - is served, exactly as on Unix, but for a different
/// reason: the kernel completes its `CreateFileW` against the waiting
/// instance, and the `connect()` the accept loop eventually issues returns
/// immediately for an instance a client already reached. The difference from a
/// socket's listen backlog is depth: a pipe holds *one* such client per
/// instance, so a second one arriving inside that window gets
/// `ERROR_PIPE_BUSY` rather than queueing. [`Stream::connect`] waits that out,
/// and the shim's bootstrap lock already stops two shims from racing here in
/// the first place.
#[derive(Debug)]
pub struct Listener {
    endpoint: Endpoint,
    instance: OwnedHandle,
}

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        Ok(Self { endpoint: endpoint.clone(), instance: create_instance(endpoint, true)? })
    }

    /// Hands the already-created first pipe instance to tokio. Must be called
    /// from inside a runtime, like its Unix counterpart.
    pub fn into_async(self) -> io::Result<AsyncListener> {
        // SAFETY: `create_instance` opened this handle with
        // `FILE_FLAG_OVERLAPPED`, which is what `NamedPipeServer` requires,
        // and `into_raw_handle` gives up this side's ownership of it.
        let server = unsafe { NamedPipeServer::from_raw_handle(self.instance.into_raw_handle()) }?;
        Ok(AsyncListener { endpoint: self.endpoint, server })
    }
}

/// The accept side, once tokio owns it.
#[derive(Debug)]
pub struct AsyncListener {
    endpoint: Endpoint,
    server: NamedPipeServer,
}

impl AsyncListener {
    /// Waits for a client and yields the instance it connected to, replacing
    /// it with a fresh one.
    ///
    /// This is the shape a named pipe forces and a listening socket hides: a
    /// pipe instance *is* the connection, so accepting means giving this one
    /// away and creating the next. The replacement is created immediately, so
    /// the window in which a connecting client sees `ERROR_PIPE_BUSY` is the
    /// few microseconds between the two - which [`Stream::connect`] waits
    /// out. The name itself never lapses: the handed-off instance still holds
    /// it throughout.
    pub async fn accept(&mut self) -> io::Result<AsyncStream> {
        self.server.connect().await?;
        let next = create_instance(&self.endpoint, false)?;
        // SAFETY: as in `into_async` - an overlapped handle whose ownership is
        // given up here.
        let next = unsafe { NamedPipeServer::from_raw_handle(next.into_raw_handle()) }?;
        Ok(std::mem::replace(&mut self.server, next))
    }
}

/// One accepted connection, as `rmcp` consumes it (`AsyncRead + AsyncWrite`).
pub type AsyncStream = NamedPipeServer;

/// Creates one server-side instance of `endpoint`'s pipe.
///
/// `first` sets `FILE_FLAG_FIRST_PIPE_INSTANCE`, which is what turns "someone
/// else already owns this name" into an error instead of quietly joining
/// their pipe as a second instance. Only the very first instance may ask for
/// it - by the time [`AsyncListener::accept`] creates a replacement this
/// process already holds one, and the flag would reject its own listener.
fn create_instance(endpoint: &Endpoint, first: bool) -> io::Result<OwnedHandle> {
    let name = endpoint.wide();
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }

    // SAFETY: `name` is a NUL-terminated wide string that outlives the call,
    // and a null security descriptor asks for the default (this user and
    // SYSTEM), which is what keeps another user's client off the pipe.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: reads this thread's last error; no arguments.
        let code = unsafe { GetLastError() };
        let err = io::Error::from_raw_os_error(code as i32);
        if code == ERROR_ACCESS_DENIED && first {
            // The `AddrInUse` of this platform, and worth saying in those
            // words: a live server holds the name. See this module's header
            // for why that can never be a leftover from a dead one.
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{endpoint} is already held by a live server: {err}"),
            ));
        }
        return Err(err);
    }
    // SAFETY: `CreateNamedPipeW` returned a fresh handle this process owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}

/// A manual-reset, initially unsignalled, unnamed event for one `Stream`'s
/// overlapped completions. Manual-reset because the kernel resets it when the
/// next operation on the same `OVERLAPPED` starts, so reusing one event across
/// a `Stream`'s whole life needs no bookkeeping here.
fn completion_event() -> io::Result<OwnedHandle> {
    // SAFETY: no pointer arguments other than the null attributes and name.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        // SAFETY: reads this thread's last error; no arguments.
        return Err(io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
    }
    // SAFETY: `CreateEventW` returned a fresh handle this process owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}
