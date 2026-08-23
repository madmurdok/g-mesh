//! The three things this project does to processes that POSIX spells with
//! signals, named once and implemented per platform: is it alive, ask it to
//! stop, make it stop, and start one detached.
//!
//! # Why Windows stops a daemon with `TerminateProcess` rather than a
//! shutdown request
//!
//! The obvious reading of "Windows has no signals" is that `SIGTERM` needs a
//! cooperative replacement - a shutdown request the daemon listens for, so it
//! can clean up the way it would have on a signal. That reading is wrong for
//! *this* daemon, and the evidence is in the daemon itself: it installs no
//! signal handler anywhere. `SIGTERM` kills it outright today. Its socket
//! file and pid file are not cleaned up by the daemon at all - they are
//! cleaned up afterwards, from outside, by `cli::stop::tidy_state_files`, and
//! its singleton `flock` is released by the kernel as part of teardown (see
//! `daemon::acquire_singleton_lock`'s note on exactly that).
//!
//! So `TerminateProcess` is not a shortcut past a cleanup step that exists on
//! Unix; it is the same abrupt teardown, and every piece of cleanup that
//! follows a `SIGTERM` today follows it unchanged. Adding a cooperative
//! shutdown path would mean the daemon behaving *differently* - and better -
//! on Windows than on the platform this project actually ships and measures,
//! which is the redesign this port is explicitly not allowed to do.
//!
//! What is genuinely lost is the escalation's meaning. On Unix `SIGTERM` can
//! be ignored, so `SIGKILL` after it distinguishes "shut down politely" from
//! "had to be killed" (`cli::stop::Stopped`). Nothing can ignore
//! `TerminateProcess`, so on Windows [`request_stop`] and [`force_stop`] are
//! the same call, the escalation never escalates, and `Stopped::Killed` is
//! unreachable. That is an honest difference in what the platform offers, not
//! a gap in this implementation.
//!
//! Windows *gains* one thing here: because a pipe name only exists while a
//! handle to it does, a terminated daemon leaves no stale endpoint behind at
//! all - the case the Unix path has to unlink around (see
//! [`crate::ipc::windows`]).

use std::process::Command;

use anyhow::Result;

/// Whether a process with this pid currently exists.
///
/// Inherently a snapshot, and pids are reused, so a caller that cares
/// (`cli::status`) corroborates it with the daemon's endpoint rather than
/// trusting a recorded pid on its own.
pub fn is_alive(pid: u32) -> bool {
    imp::is_alive(pid)
}

/// Asks a process to stop: `SIGTERM` on Unix, and on Windows the only thing
/// there is (see this module's header).
///
/// A process that is already gone is the outcome this asks for, not a
/// failure, and both implementations report it that way.
pub fn request_stop(pid: u32) -> Result<()> {
    imp::request_stop(pid)
}

/// Makes a process stop, for a process that ignored [`request_stop`].
pub fn force_stop(pid: u32) -> Result<()> {
    imp::force_stop(pid)
}

/// Detaches a child so it outlives the process that spawned it and is not
/// reached by signals aimed at the spawner's group.
///
/// The caller still has to redirect stdio - detachment here is only about the
/// process's relationship to its parent's console and signal group.
pub fn detach(command: &mut Command) -> &mut Command {
    imp::detach(command)
}

/// Stops any child this process spawns from receiving copies of its standard
/// handles, whatever the child's own stdio is set to.
///
/// This exists because `Stdio::null()` does not say what it appears to say on
/// Windows. It sets what the child's STARTUPINFO names, while `CreateProcess`
/// is called with `bInheritHandles = TRUE` and hands over **every inheritable
/// handle in the parent** regardless. A shim's stdin and stdout are the MCP
/// channel and are inheritable, having themselves been inherited from the
/// client - so a daemon spawned with all three stdio streams pointed at NUL
/// still ended up holding the client's pipe, and the client could not see the
/// shim close: EOF never came, because a writer it had never heard of was
/// still alive (GM-251).
///
/// A no-op on Unix, where `CLOEXEC` already means nothing crosses an exec that
/// was not asked for. Same shape as `ipc::Endpoint::check_length`'s Windows
/// no-op: the caller states the intent unconditionally and the platform that
/// has nothing to do does nothing.
///
/// Process-wide and permanent, deliberately. The invariant is "this process
/// never hands its channel to a child", not "not on this one spawn", and a
/// per-spawn version would be a flag to remember at every future call site.
pub fn keep_our_stdio_from_children() {
    imp::keep_our_stdio_from_children()
}

#[cfg(unix)]
mod imp {
    use anyhow::{Context, Result};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    pub fn is_alive(pid: u32) -> bool {
        // `kill(pid, 0)` performs the usual permission and existence checks
        // without delivering anything. `EPERM` counts as alive: the process is
        // there, this user just may not signal it - reporting it as gone would
        // be the more misleading of the two answers.
        //
        // SAFETY: `kill` with signal 0 only inspects; it cannot affect this
        // process, and no pointers are involved.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    pub fn request_stop(pid: u32) -> Result<()> {
        send_signal(pid, libc::SIGTERM)
    }

    pub fn force_stop(pid: u32) -> Result<()> {
        send_signal(pid, libc::SIGKILL)
    }

    pub fn send_signal(pid: u32, signal: libc::c_int) -> Result<()> {
        // SAFETY: `kill` takes no pointers, and the only process affected is
        // the one named by `pid`.
        if unsafe { libc::kill(pid as libc::pid_t, signal) } == 0 {
            return Ok(());
        }

        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // It exited between the liveness check and here: the outcome this
            // call was asking for, reached without it.
            Some(libc::ESRCH) => Ok(()),
            _ => Err(err).with_context(|| format!("failed to signal pid {pid}")),
        }
    }

    pub fn detach(command: &mut Command) -> &mut Command {
        // Its own process group keeps signals aimed at the client's process
        // group away from it.
        command.process_group(0)
    }

    /// Nothing to do: Rust opens its file descriptors `CLOEXEC`, so a child
    /// gets what `Command`'s stdio names and nothing else.
    pub fn keep_our_stdio_from_children() {}
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    use windows_sys::Win32::Foundation::{
        GetLastError, SetHandleInformation, ERROR_ACCESS_DENIED, HANDLE_FLAG_INHERIT, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
        PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    /// The exit code a terminated daemon is recorded with. Nothing reads it -
    /// no caller of `cli::stop` waits on the process it stopped - but a value
    /// has to be supplied, and 1 says "did not exit of its own accord" to
    /// anything that ever looks.
    const TERMINATION_EXIT_CODE: u32 = 1;

    pub fn is_alive(pid: u32) -> bool {
        // Two questions, because a handle can still be opened for a process
        // that has already exited but whose handles are not all closed:
        // whether a process object exists at all, and whether it is still
        // running. `ERROR_ACCESS_DENIED` counts as alive for the same reason
        // `EPERM` does on Unix - the process is there, this token just may not
        // inspect it.
        let Some(handle) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
            // SAFETY: reads this thread's last error; no arguments.
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        };

        let mut code: u32 = 0;
        // SAFETY: `handle` is live for the call, and `code` is a valid `u32`
        // this frame owns.
        let queried = unsafe { GetExitCodeProcess(raw(&handle), &mut code) };
        queried != 0 && code == STILL_ACTIVE as u32
    }

    pub fn request_stop(pid: u32) -> Result<()> {
        terminate(pid)
    }

    pub fn force_stop(pid: u32) -> Result<()> {
        terminate(pid)
    }

    fn terminate(pid: u32) -> Result<()> {
        let Some(handle) = open(pid, PROCESS_TERMINATE) else {
            // No process object to open: it exited between the liveness check
            // and here, which is the outcome this call was asking for. Matches
            // the Unix implementation's reading of `ESRCH`.
            return Ok(());
        };
        // SAFETY: `handle` was opened for termination and is live for the call.
        if unsafe { TerminateProcess(raw(&handle), TERMINATION_EXIT_CODE) } != 0 {
            return Ok(());
        }

        let err = std::io::Error::last_os_error();
        // A process that finished on its own between `open` and here cannot be
        // terminated, and must not be reported as a failure to stop it.
        if !is_alive(pid) {
            return Ok(());
        }
        Err(err).with_context(|| format!("failed to terminate pid {pid}"))
    }

    pub fn detach(command: &mut Command) -> &mut Command {
        // `DETACHED_PROCESS` is the console half of Unix detachment: the child
        // gets no console at all rather than inheriting the spawner's, so
        // nothing it writes can reach the MCP client's terminal and closing
        // that terminal cannot reach it. `CREATE_NEW_PROCESS_GROUP` is the
        // signal half - it is what stops a Ctrl-C (a `CTRL_BREAK_EVENT`/
        // `CTRL_C_EVENT` sent to the spawner's group) from also reaching the
        // daemon, which is exactly what `process_group(0)` buys on Unix.
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
    }

    pub fn keep_our_stdio_from_children() {
        // Failure is ignored on purpose, per handle. A standard handle can be
        // absent or already non-inheritable - a service with no console, a
        // redirected stream - and neither is a reason to refuse to start. The
        // call is a tightening; where there is nothing to tighten there is
        // nothing to report.
        for handle in [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            // SAFETY: the handle comes from this process's own standard
            // stream, is valid for the duration of the call, and only its
            // inheritance flag is written - nothing is read through it and no
            // ownership changes hands.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
    }

    fn open(pid: u32, rights: PROCESS_ACCESS_RIGHTS) -> Option<OwnedHandle> {
        // SAFETY: no pointer arguments; a null return means failure, which is
        // checked before the handle is adopted.
        let handle = unsafe { OpenProcess(rights, 0, pid) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: `OpenProcess` returned a fresh handle this process owns and
        // has not handed to anything else; `OwnedHandle` closes it on drop.
        Some(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
    }

    fn raw(handle: &OwnedHandle) -> windows_sys::Win32::Foundation::HANDLE {
        use std::os::windows::io::AsRawHandle;
        handle.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_current_process_is_alive() {
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn a_reaped_child_is_not_alive() {
        let mut child =
            Command::new("sh").arg("-c").arg("exit 0").spawn().expect("failed to spawn a test process");
        let pid = child.id();
        child.wait().expect("failed to reap the test process");

        assert!(!is_alive(pid), "pid {pid} was reaped and must not read as alive");
    }
}
