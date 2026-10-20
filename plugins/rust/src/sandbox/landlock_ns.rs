//! The Linux backend: the plugin binary is its own trampoline.
//!
//! [`confine`] rewrites the server's argv to
//! `<launcher> __g-mesh-sandbox --layers <layers> --root <root> --allow <prefix>… -- <server> <args…>`.
//! [`trampoline`], called first thing in the plugin's `main`, sets up the
//! requested layers in that fresh single-threaded process and execs the
//! server in place, so the spawned pid is the server's and every child
//! inherits the layers:
//!
//! - **mount**: a user + mount namespace in which every mount is `noexec`
//!   except the OS and toolchain prefixes, and the project root is `noexec`
//!   again even inside an allowed prefix. This is what keeps a shared object
//!   under the root from being mapped executable (`LD_PRELOAD`, `dlopen`).
//! - **landlock**: a Landlock domain allowing exec only under the same
//!   prefixes, and no TCP bind or connect. Landlock does not check
//!   `mmap(PROT_EXEC)`, so on its own it blocks execs, not library loads.
//!
//! [`confine`] proves a tier with a `--version` preflight, trying the full
//! tier first and one fallback chosen by [`next_rung`]
//! ([ADR 0039](../../../../docs/adr/0039-rust-server-runs-under-an-os-sandbox.md)).

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use g_mesh_plugin_sdk::lsp::{self, SemanticConfig, PROBE_BUDGET};

use super::{SandboxOutcome, SandboxPolicy};

/// The first argument that makes the plugin binary a trampoline.
pub(crate) const TRAMPOLINE_ARG: &str = "__g-mesh-sandbox";

/// OS prefixes whose binaries and libraries the tree may exec and map; one
/// that does not exist is skipped, a symlinked one (merged `/usr`) is
/// followed.
pub(crate) const OS_EXEC: [&str; 8] =
    ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/nix/store"];

/// Trampoline exit codes, each naming the step that failed.
pub(crate) const EXIT_USAGE: i32 = 70;
pub(crate) const EXIT_MOUNT: i32 = 71;
pub(crate) const EXIT_LANDLOCK: i32 = 72;
pub(crate) const EXIT_EXEC: i32 = 73;

/// Which layers the trampoline sets up. It sets up exactly these or fails:
/// it never drops one on its own, so a respawn runs the tier the preflight
/// proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layers {
    /// Mount namespace and Landlock.
    Full,
    /// Mount namespace only (no Landlock in this kernel).
    Mount,
    /// Landlock only (no usable user namespace).
    Landlock,
}

impl Layers {
    pub(crate) fn as_arg(self) -> &'static str {
        match self {
            Layers::Full => "mount,landlock",
            Layers::Mount => "mount",
            Layers::Landlock => "landlock",
        }
    }

    pub(crate) fn from_arg(arg: &str) -> Option<Self> {
        [Layers::Full, Layers::Mount, Layers::Landlock].into_iter().find(|layers| layers.as_arg() == arg)
    }

    fn mount(self) -> bool {
        matches!(self, Layers::Full | Layers::Mount)
    }

    fn landlock(self) -> bool {
        matches!(self, Layers::Full | Layers::Landlock)
    }
}

/// The tier to try after `tried` failed its preflight with the trampoline's
/// `exit` code: Landlock missing after the full tier -> mount only; the
/// mount layer failing after the full tier -> Landlock only. Anything else
/// (a usage or exec failure, a timeout, a failed fallback) ends the ladder.
pub(crate) fn next_rung(tried: Layers, exit: Option<i32>) -> Option<Layers> {
    match (tried, exit) {
        (Layers::Full, Some(EXIT_LANDLOCK)) => Some(Layers::Mount),
        (Layers::Full, Some(EXIT_MOUNT)) => Some(Layers::Landlock),
        _ => None,
    }
}

/// The trampoline's argv for `policy` and `layers`, up to and including
/// `--`. `Err` names a path that is not UTF-8, which the server's
/// `Vec<String>` args cannot carry.
pub(crate) fn trampoline_args(policy: &SandboxPolicy, layers: Layers) -> Result<Vec<String>, String> {
    let utf8 = |path: &Path| {
        path.to_str().map(str::to_string).ok_or_else(|| format!("{} is not UTF-8", path.display()))
    };
    let mut args = vec![
        TRAMPOLINE_ARG.to_string(),
        "--layers".to_string(),
        layers.as_arg().to_string(),
        "--root".to_string(),
        utf8(&policy.project_root)?,
    ];
    for prefix in &policy.toolchain {
        args.push("--allow".to_string());
        args.push(utf8(prefix)?);
    }
    args.push("--".to_string());
    Ok(args)
}

/// The trampoline's exit code in a failed probe's error text
/// (`` `--version` exited exit status: N (…) ``).
pub(crate) fn exit_code(detail: &str) -> Option<i32> {
    let rest = &detail[detail.find("exit status: ")? + "exit status: ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The reason for a failed preflight, from the probe's error text.
pub(crate) fn failure(detail: &str) -> String {
    match exit_code(detail) {
        Some(EXIT_USAGE) => format!("the sandbox trampoline rejected its arguments: {detail}"),
        Some(EXIT_MOUNT) => format!("the noexec mount namespace cannot be set up: {detail}"),
        Some(EXIT_LANDLOCK) => format!("Landlock cannot be applied: {detail}"),
        Some(EXIT_EXEC) => format!("the sandbox could not exec rust-analyzer: {detail}"),
        _ => format!("the confined rust-analyzer did not start: {detail}"),
    }
}

/// See [`super::confine`]. `launcher` is the plugin binary that runs the
/// trampoline (`/proc/self/exe` in the plugin).
pub(crate) fn confine(
    config: &mut SemanticConfig,
    policy: &SandboxPolicy,
    launcher: &Path,
) -> SandboxOutcome {
    let server = config.command.to_string_lossy().into_owned();
    let mut layers = Layers::Full;
    let mut failures: Vec<String> = Vec::new();
    loop {
        let mut wrapped = match trampoline_args(policy, layers) {
            Ok(args) => args,
            Err(reason) => {
                return SandboxOutcome::Failed(format!("cannot pass a path to the Linux sandbox: {reason}"))
            }
        };
        wrapped.push(server.clone());
        match lsp::probe(launcher, &wrapped, PROBE_BUDGET) {
            Ok(_) => {
                wrapped.append(&mut config.args);
                config.command = launcher.to_path_buf();
                config.args = wrapped;
                let earlier = failures.join("; ");
                return match layers {
                    Layers::Full => SandboxOutcome::Confined { backend: "noexec mounts + Landlock" },
                    Layers::Mount => {
                        g_mesh_plugin_sdk::log_line!(
                            "[rust] semantic tier: sandbox: Landlock unavailable ({earlier})"
                        );
                        SandboxOutcome::Confined { backend: "noexec mounts" }
                    }
                    Layers::Landlock => SandboxOutcome::Partial { backend: "Landlock", gap: earlier },
                };
            }
            Err(err) => {
                let detail = format!("{err:#}");
                failures.push(failure(&detail));
                match next_rung(layers, exit_code(&detail)) {
                    Some(next) => layers = next,
                    None => return SandboxOutcome::Failed(failures.join("; ")),
                }
            }
        }
    }
}

/// The parsed trampoline argv.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub(crate) layers: Layers,
    pub(crate) root: PathBuf,
    pub(crate) allow: Vec<PathBuf>,
    pub(crate) server: PathBuf,
    pub(crate) args: Vec<OsString>,
}

/// Parses the argv after [`TRAMPOLINE_ARG`]. The server path must be
/// absolute: the trampoline never searches `PATH`.
pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, String> {
    let mut args = args.into_iter();
    let mut layers = None;
    let mut root = None;
    let mut allow = Vec::new();
    loop {
        let flag = args.next().ok_or("no `--` before the server")?;
        let flag = flag.to_str().ok_or("a non-UTF-8 option")?.to_string();
        if flag == "--" {
            break;
        }
        let value = args.next().ok_or_else(|| format!("{flag} has no value"))?;
        match flag.as_str() {
            "--layers" => {
                let value = value.to_str().unwrap_or("");
                layers = Some(Layers::from_arg(value).ok_or_else(|| format!("unknown --layers {value}"))?);
            }
            "--root" => root = Some(PathBuf::from(value)),
            "--allow" => allow.push(PathBuf::from(value)),
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    let server = PathBuf::from(args.next().ok_or("no server after `--`")?);
    if !server.is_absolute() {
        return Err(format!("the server path {} is not absolute", server.display()));
    }
    Ok(Invocation {
        layers: layers.ok_or("no --layers")?,
        root: root.ok_or("no --root")?,
        allow,
        server,
        args: args.collect(),
    })
}

/// Runs the trampoline when `args` (the whole argv) is a trampoline call:
/// sets up the layers and execs the server, never returning on success.
/// `Some(code)` is the exit code of a failure (its reason is on stderr);
/// `None` means `args` is not a trampoline call.
///
/// Must run before the process starts a thread: `unshare(CLONE_NEWUSER)`
/// fails in a multi-threaded process.
pub fn trampoline(args: impl IntoIterator<Item = OsString>) -> Option<i32> {
    let mut args = args.into_iter();
    let _argv0 = args.next();
    let mut rest = args.peekable();
    if rest.peek().map(|arg| arg.as_os_str()) != Some(TRAMPOLINE_ARG.as_ref()) {
        return None;
    }
    rest.next();
    let invocation = match parse(rest) {
        Ok(invocation) => invocation,
        Err(reason) => {
            g_mesh_plugin_sdk::log_line!("g-mesh sandbox: usage: {reason}");
            return Some(EXIT_USAGE);
        }
    };
    let allowed = allowed_prefixes(&invocation.allow);
    if invocation.layers.mount() {
        if let Err(reason) = mount_layer(&invocation.root, &allowed) {
            g_mesh_plugin_sdk::log_line!("g-mesh sandbox: mount layer: {reason}");
            return Some(EXIT_MOUNT);
        }
    }
    if invocation.layers.landlock() {
        if let Err(reason) = landlock_layer(&allowed) {
            g_mesh_plugin_sdk::log_line!("g-mesh sandbox: Landlock layer: {reason}");
            return Some(EXIT_LANDLOCK);
        }
    }
    let error = std::process::Command::new(&invocation.server).args(&invocation.args).exec();
    g_mesh_plugin_sdk::log_line!("g-mesh sandbox: exec {}: {error}", invocation.server.display());
    Some(EXIT_EXEC)
}

/// The OS prefixes that exist, canonicalized, then `extra` as given,
/// deduplicated.
fn allowed_prefixes(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut prefixes: Vec<PathBuf> = Vec::new();
    let os = OS_EXEC.iter().filter_map(|prefix| std::fs::canonicalize(prefix).ok());
    for prefix in os.chain(extra.iter().cloned()) {
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    }
    prefixes
}

/// The mount layer's per-path steps, in the order they are applied:
/// `(path, exec)` for every allowed prefix (`exec`) and the root (not),
/// ascending by path length so the most specific applies last. At equal
/// length (the root is itself an allowed prefix) the root comes last.
pub(crate) fn mount_plan(root: &Path, allowed: &[PathBuf]) -> Vec<(PathBuf, bool)> {
    let mut plan: Vec<(PathBuf, bool)> = allowed.iter().map(|prefix| (prefix.clone(), true)).collect();
    plan.push((root.to_path_buf(), false));
    plan.sort_by_key(|(path, exec)| (path.as_os_str().len(), !exec));
    plan
}

fn cstring(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("{} holds a NUL byte", path.display()))
}

fn errno(step: &str) -> String {
    format!("{step}: {}", std::io::Error::last_os_error())
}

/// `mount_setattr(path, AT_RECURSIVE)` setting or clearing `noexec` on the
/// mount at `path` and every mount beneath it.
fn set_noexec(path: &CString, noexec: bool) -> Result<(), String> {
    let mut attr: libc::mount_attr = unsafe { std::mem::zeroed() };
    if noexec {
        attr.attr_set = libc::MOUNT_ATTR_NOEXEC;
    } else {
        attr.attr_clr = libc::MOUNT_ATTR_NOEXEC;
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_RECURSIVE as libc::c_uint,
            &attr as *const libc::mount_attr,
            std::mem::size_of::<libc::mount_attr>(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(errno(&format!("mount_setattr {}", path.to_string_lossy())))
    }
}

/// Enters a new user + mount namespace (uid and gid mapped to themselves)
/// in which every mount is `noexec` except under `allowed`, and `root` is
/// `noexec` again wherever it lies. Mounts never propagate out.
fn mount_layer(root: &Path, allowed: &[PathBuf]) -> Result<(), String> {
    let plan = mount_plan(root, allowed);
    let plan: Vec<(CString, bool)> =
        plan.iter().map(|(path, exec)| cstring(path).map(|path| (path, *exec))).collect::<Result<_, _>>()?;
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let cwd = std::env::current_dir().ok();

    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
        return Err(errno("unshare(CLONE_NEWUSER | CLONE_NEWNS)"));
    }
    std::fs::write("/proc/self/setgroups", "deny").map_err(|err| format!("/proc/self/setgroups: {err}"))?;
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))
        .map_err(|err| format!("/proc/self/uid_map: {err}"))?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))
        .map_err(|err| format!("/proc/self/gid_map: {err}"))?;

    let slash = cstring(Path::new("/"))?;
    let none: *const libc::c_char = std::ptr::null();
    if unsafe { libc::mount(none, slash.as_ptr(), none, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) }
        != 0
    {
        return Err(errno("mount / private"));
    }
    set_noexec(&slash, true)?;
    for (path, exec) in &plan {
        let bound = unsafe {
            libc::mount(path.as_ptr(), path.as_ptr(), none, libc::MS_BIND | libc::MS_REC, std::ptr::null())
        };
        if bound != 0 {
            return Err(errno(&format!("bind {}", path.to_string_lossy())));
        }
        set_noexec(path, !exec)?;
    }
    // The working directory still resolves through the mounts it was
    // entered under; re-entering it resolves it through the new binds.
    if let Some(cwd) = cwd {
        let _ = std::env::set_current_dir(cwd);
    }
    Ok(())
}

/// Restricts this process and its descendants with Landlock: exec only
/// under `allowed`, no TCP bind or connect (ABI >= 4), no abstract unix
/// socket or signal outside the domain (ABI >= 6). `Err` when the kernel
/// enforces none of it.
fn landlock_layer(allowed: &[PathBuf]) -> Result<(), String> {
    use landlock::{
        path_beneath_rules, AccessFs, AccessNet, LandlockStatus, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, Scope,
    };
    let status = Ruleset::default()
        .handle_access(AccessFs::Execute)
        .and_then(|ruleset| ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp))
        .and_then(|ruleset| ruleset.scope(Scope::AbstractUnixSocket | Scope::Signal))
        .and_then(|ruleset| ruleset.create())
        .and_then(|ruleset| ruleset.add_rules(path_beneath_rules(allowed, AccessFs::Execute)))
        .and_then(|ruleset| ruleset.restrict_self())
        .map_err(|err| err.to_string())?;
    if status.ruleset == RulesetStatus::NotEnforced {
        let why = match status.landlock {
            LandlockStatus::NotImplemented => "not built into this kernel".to_string(),
            LandlockStatus::NotEnabled => "built in but not enabled".to_string(),
            other => format!("not enforced ({other:?})"),
        };
        return Err(format!("Landlock unavailable: {why}"));
    }
    Ok(())
}
