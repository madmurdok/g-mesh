//! The things this project does to processes that POSIX spells with
//! signals, named once and implemented per platform: is it alive, ask it to
//! stop, make it stop, start one detached, and (Unix only) the same for a
//! detached daemon's whole process group.
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

use std::process::{Child, Command};
use std::sync::Mutex;

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

/// The process group to signal in place of `pid`, when `pid` leads a group of
/// its own: `Some(pid)` for a daemon `detach` started (it is its group's
/// leader, and its plugins, their language servers and those servers'
/// `cargo`/`rustc` all inherit that group). `None` when `pid` is not a group
/// leader - a daemon run by hand in a shell job, or by a test harness without
/// `detach` - or when its group is the caller's own, so signalling the group
/// could never reach a process the caller did not mean (GM-524 D2). Always
/// `None` on Windows, which has no process groups in this sense.
pub fn own_group_of(pid: u32) -> Option<u32> {
    imp::own_group_of(pid)
}

/// [`request_stop`] for every member of process group `pgid`.
pub fn request_stop_group(pgid: u32) -> Result<()> {
    imp::signal_group(pgid, imp::TERMINATE)
}

/// [`force_stop`] for every member of process group `pgid`.
pub fn force_stop_group(pgid: u32) -> Result<()> {
    imp::signal_group(pgid, imp::KILL)
}

/// Whether process group `pgid` still has any member.
pub fn group_is_alive(pgid: u32) -> bool {
    imp::group_is_alive(pgid)
}

/// What [`kill_own_group`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupSweep {
    /// This process does not lead its own group, so the group belongs to
    /// whatever started it (a shell job, a test harness) and was left alone.
    /// Always the answer on Windows.
    NotLeader,
    /// `terminated` members were sent `SIGTERM`; `killed` of them were still
    /// there after the grace and were sent `SIGKILL`.
    Swept { terminated: usize, killed: usize },
}

/// Ends every other process in this process's group: `SIGTERM`, up to
/// `grace` for them to go, then `SIGKILL` to whatever is left. This process
/// itself is never signalled - it has no `SIGTERM` handler, so `killpg` would
/// end it before it finished its own teardown. Members are found by listing
/// the processes (`sysinfo`) and keeping those whose `getpgid` is ours, so a
/// member forked after the `SIGTERM` is still caught by the `SIGKILL` round's
/// fresh listing.
///
/// A no-op unless this process leads its group (see [`GroupSweep::NotLeader`]):
/// a daemon is a group leader only when `detach` started it, which is the one
/// case where the whole group is its own.
pub fn kill_own_group(grace: std::time::Duration) -> GroupSweep {
    imp::kill_own_group(grace)
}

/// A fresh snapshot of every process on the system, with `refresh` saying
/// which per-process facts to read beyond those `sysinfo` always reads (pid,
/// parent, status). Threads are never listed: on Linux `sysinfo` otherwise
/// lists every thread as a process of its own, parented to its process and
/// reporting that whole process's memory and group. Iterate it through
/// [`is_process`], which drops any thread entry that still appears.
pub fn process_snapshot(refresh: sysinfo::ProcessRefreshKind) -> sysinfo::System {
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, refresh.without_tasks());
    system
}

/// Whether a snapshot entry is a process rather than one of a process's
/// threads. Always true outside Linux, where `sysinfo` lists no threads.
pub fn is_process(process: &sysinfo::Process) -> bool {
    process.thread_kind().is_none()
}

/// The one lock every plugin spawn is taken under - see [`spawn_serialized`].
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Spawns `command` while holding a process-wide lock, so no two plugin
/// spawns in this process overlap (GM-397).
///
/// **Unverified hardening.** Nothing reproduces the race this closes and no
/// test can pin it; it is here because the standard library's source shows
/// it, not because it was observed (`docs/architecture/plugin-lifetime.md`
/// §1.3, §2 item 5, §5 Q1). On targets without `pipe2` - macOS among them -
/// `Command::spawn` creates each stdio pipe with `pipe()` and only then marks
/// it `FD_CLOEXEC`, and its `posix_spawn` path does not set
/// `POSIX_SPAWN_CLOEXEC_DEFAULT`. A spawn on another thread that lands in that
/// window hands the new pipe end to an unrelated child. For a plugin that is
/// fatal to its lifeline: a sibling holding the write end of plugin A's stdin
/// means A never sees EOF when the daemon dies, and two plugins can each hold
/// the other's. Serializing the spawns - pipe creation included, since both
/// happen inside `Command::spawn` - removes the overlap. Spawns are rare, so
/// the lock costs nothing measurable.
///
/// A poisoned lock is taken anyway: it guards no data, only an interval.
pub fn spawn_serialized(command: &mut Command) -> std::io::Result<Child> {
    let _guard = SPAWN_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    command.spawn()
}

#[cfg(unix)]
mod imp {
    use anyhow::{Context, Result};
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

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

    pub const TERMINATE: libc::c_int = libc::SIGTERM;
    pub const KILL: libc::c_int = libc::SIGKILL;

    /// How often [`kill_own_group`] re-lists the group while it waits.
    const GROUP_POLL: Duration = Duration::from_millis(50);

    fn pgid_of(pid: u32) -> Option<u32> {
        // SAFETY: `getpgid` takes no pointers and only reads.
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        (pgid > 0).then_some(pgid as u32)
    }

    fn own_pgid() -> u32 {
        // SAFETY: `getpgrp` takes no arguments and cannot fail.
        unsafe { libc::getpgrp() as u32 }
    }

    pub fn own_group_of(pid: u32) -> Option<u32> {
        (pgid_of(pid) == Some(pid) && pid != own_pgid()).then_some(pid)
    }

    pub fn signal_group(pgid: u32, signal: libc::c_int) -> Result<()> {
        // Never a group this process is in, whatever the caller passed:
        // `killpg` on it would end the caller too.
        if pgid <= 1 || pgid == own_pgid() {
            anyhow::bail!("refusing to signal process group {pgid}: it is not a separate group");
        }
        // SAFETY: `killpg` takes no pointers; it affects only group `pgid`.
        if unsafe { libc::killpg(pgid as libc::pid_t, signal) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // The group has no members left: what the signal was for.
            Some(libc::ESRCH) => Ok(()),
            _ => Err(err).with_context(|| format!("failed to signal process group {pgid}")),
        }
    }

    pub fn group_is_alive(pgid: u32) -> bool {
        if pgid <= 1 {
            return false;
        }
        // SAFETY: signal 0 only inspects; no pointers.
        if unsafe { libc::killpg(pgid as libc::pid_t, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Every live process in group `pgid` except `me`. Zombies are left out:
    /// one cannot be signalled away, and waiting on it would only spend the
    /// whole grace.
    ///
    /// Processes only, never threads (see [`super::process_snapshot`]): a
    /// thread's id shares its process's group, so this daemon's own threads
    /// would pass the `pid != me` filter, and `kill(tid, SIGTERM)` is
    /// delivered to the whole thread group - the daemon would end itself
    /// mid-sweep, before signalling the rest of its group and before
    /// releasing its pid file.
    fn group_members(pgid: u32, me: u32) -> Vec<u32> {
        use sysinfo::{ProcessRefreshKind, ProcessStatus};
        let system = super::process_snapshot(ProcessRefreshKind::nothing());
        system
            .processes()
            .iter()
            .filter(|(_, process)| super::is_process(process))
            .filter(|(_, process)| process.status() != ProcessStatus::Zombie)
            .map(|(pid, _)| pid.as_u32())
            .filter(|&pid| pid != me && pgid_of(pid) == Some(pgid))
            .collect()
    }

    /// Signals each of `pids` that is still in group `pgid` - re-checked
    /// right before the signal, so a member that exited since the listing and
    /// whose pid was reused is not hit.
    fn signal_members(pids: &[u32], pgid: u32, signal: libc::c_int) {
        for &pid in pids {
            if pgid_of(pid) == Some(pgid) {
                let _ = send_signal(pid, signal);
            }
        }
    }

    pub fn kill_own_group(grace: Duration) -> super::GroupSweep {
        let me = std::process::id();
        let pgid = own_pgid();
        if pgid != me {
            return super::GroupSweep::NotLeader;
        }
        let members = group_members(pgid, me);
        if members.is_empty() {
            return super::GroupSweep::Swept { terminated: 0, killed: 0 };
        }
        signal_members(&members, pgid, libc::SIGTERM);
        let deadline = Instant::now() + grace;
        let mut left = group_members(pgid, me);
        while !left.is_empty() && Instant::now() < deadline {
            std::thread::sleep(GROUP_POLL);
            left = group_members(pgid, me);
        }
        signal_members(&left, pgid, libc::SIGKILL);
        super::GroupSweep::Swept { terminated: members.len(), killed: left.len() }
    }
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

    // Process groups: Unix-only (GM-524 D2). `CREATE_NEW_PROCESS_GROUP` is a
    // console-signal group, not something `TerminateProcess` can address, and
    // the equivalent (a job object the daemon puts itself and its children in)
    // is out of this task's scope. `own_group_of` answering `None` keeps every
    // caller on the per-pid path, so the two below are never reached.
    pub const TERMINATE: i32 = 0;
    pub const KILL: i32 = 0;

    pub fn own_group_of(_pid: u32) -> Option<u32> {
        None
    }

    pub fn signal_group(pgid: u32, _signal: i32) -> Result<()> {
        anyhow::bail!("process group {pgid} cannot be signalled on Windows")
    }

    pub fn group_is_alive(_pgid: u32) -> bool {
        false
    }

    pub fn kill_own_group(_grace: std::time::Duration) -> super::GroupSweep {
        super::GroupSweep::NotLeader
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
