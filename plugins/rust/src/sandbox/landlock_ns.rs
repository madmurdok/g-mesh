//! The Linux backend: the plugin binary is its own trampoline.
//!
//! [`confine`] rewrites the server's argv to
//! `<launcher> __g-mesh-sandbox --layers <layers> --root <root> --allow <prefix>… --write <prefix>… -- <server> <args…>`.
//! [`trampoline`], called first thing in the plugin's `main`, sets up the
//! requested layers in that fresh single-threaded process and execs the
//! server in place, so the spawned pid is the server's and every child
//! inherits the layers:
//!
//! - **mount**: a user + mount + network namespace. Every mount is `noexec`
//!   except the OS and toolchain prefixes, and read-only except the
//!   writable prefixes (the server's temp dir, the cargo caches); the
//!   project root is `noexec` and read-only again even inside an allowed
//!   prefix. This is what keeps a shared object under the root from being
//!   mapped executable (`LD_PRELOAD`, `dlopen`). The network namespace has
//!   only a down loopback, so no TCP, UDP or abstract unix socket connects.
//! - **landlock**: a Landlock domain allowing exec only under the same
//!   prefixes, writes only under the writable prefixes and `/dev`, no TCP
//!   bind or connect, and no connect to a pathname unix socket (ABI >= 9).
//!   Landlock does not check `mmap(PROT_EXEC)`, so on its own it blocks
//!   execs, not library loads; it never covers UDP.
//!
//! In every tier the trampoline then drops all capabilities and sets
//! `no_new_privs`, so the server cannot undo a layer (root in its user
//! namespace could otherwise remount the root `exec`).
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

/// OS prefixes the tree may write in the Landlock layer: device nodes
/// (`/dev/null`, `/dev/tty`). The mount layer needs none: a device node
/// ignores a read-only mount.
pub(crate) const OS_WRITE: [&str; 1] = ["/dev"];

/// Trampoline exit codes, each naming the step that failed.
pub(crate) const EXIT_USAGE: i32 = 70;
pub(crate) const EXIT_MOUNT: i32 = 71;
pub(crate) const EXIT_LANDLOCK: i32 = 72;
pub(crate) const EXIT_EXEC: i32 = 73;
pub(crate) const EXIT_CAPS: i32 = 74;

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
/// (a usage, capability-drop or exec failure, a timeout, a failed fallback)
/// ends the ladder.
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
    for prefix in &policy.writable {
        args.push("--write".to_string());
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
        Some(EXIT_CAPS) => format!("the sandbox could not drop its capabilities: {detail}"),
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
                    Layers::Landlock => {
                        let root = std::fs::canonicalize(&policy.project_root)
                            .unwrap_or_else(|_| policy.project_root.clone());
                        let mut gaps = vec![earlier];
                        if let Some(prefix) = prefix_over(&root, &allowed_prefixes(&policy.toolchain)) {
                            gaps.push(format!(
                                "exec of project binaries is not blocked either: the project root {} lies \
                                 under the allowed prefix {}",
                                root.display(),
                                prefix.display()
                            ));
                        }
                        gaps.extend(landlock_gaps(landlock_abi(), &root, &policy.writable));
                        SandboxOutcome::Partial { backend: "Landlock", gap: gaps.join("; ") }
                    }
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
    pub(crate) write: Vec<PathBuf>,
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
    let mut write = Vec::new();
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
            "--write" => write.push(PathBuf::from(value)),
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
        write,
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
        if let Err(reason) = mount_layer(&invocation.root, &allowed, &invocation.write) {
            g_mesh_plugin_sdk::log_line!("g-mesh sandbox: mount layer: {reason}");
            return Some(EXIT_MOUNT);
        }
    }
    if invocation.layers.landlock() {
        if let Err(reason) = landlock_layer(&allowed, &invocation.write) {
            g_mesh_plugin_sdk::log_line!("g-mesh sandbox: Landlock layer: {reason}");
            return Some(EXIT_LANDLOCK);
        }
    }
    if let Err(reason) = drop_capabilities() {
        g_mesh_plugin_sdk::log_line!("g-mesh sandbox: capabilities: {reason}");
        return Some(EXIT_CAPS);
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

/// The allowed prefix `root` lies under, if any: Landlock grants exec
/// beneath a prefix and has no deny inside a grant, so in the Landlock-only
/// tier the project's own binaries are exec'able there.
pub(crate) fn prefix_over<'a>(root: &Path, allowed: &'a [PathBuf]) -> Option<&'a PathBuf> {
    allowed.iter().find(|prefix| root.starts_with(prefix))
}

/// What the Landlock-only tier leaves open at Landlock ABI `abi`, for a
/// `root` and the `writable` prefixes, one clause each, for the `Partial`
/// gap: UDP and (below ABI 9) pathname unix sockets always or by ABI, TCP
/// below ABI 4, truncation below ABI 3, and writes under a root that lies
/// under a writable prefix (Landlock has no deny inside a grant).
pub(crate) fn landlock_gaps(abi: i64, root: &Path, writable: &[PathBuf]) -> Vec<String> {
    let mut gaps = Vec::new();
    if abi >= 9 {
        gaps.push("UDP connects are not blocked".to_string());
    } else {
        gaps.push(format!(
            "UDP and unix-socket connects are not blocked (Landlock ABI {abi} < 9 checks no pathname socket)"
        ));
    }
    if abi < 4 {
        gaps.push(format!("TCP is not blocked (Landlock ABI {abi} < 4)"));
    }
    if abi < 3 {
        gaps.push(format!("truncation is not blocked (Landlock ABI {abi} < 3)"));
    }
    if let Some(prefix) = prefix_over(root, writable) {
        gaps.push(format!(
            "writes under the root are not blocked: it lies under the writable prefix {}",
            prefix.display()
        ));
    }
    gaps
}

/// The Landlock ABI version this kernel reports, 0 without Landlock.
pub fn landlock_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    abi.max(0)
}

/// One step of the mount layer: bind `path` on itself, then make it (and
/// every mount beneath it) executable when `exec` and writable when `write`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountStep {
    pub(crate) path: PathBuf,
    pub(crate) exec: bool,
    pub(crate) write: bool,
}

/// The mount layer's per-path steps, in the order they are applied: one for
/// every exec prefix, every writable prefix and the root, ascending by path
/// length so the most specific applies last; at equal length the root comes
/// last. A step's two flags are decided independently by the most specific
/// rule over its path: exec from the `exec` prefixes against the root,
/// write from the `write` prefixes against the root; the root wins a tie,
/// and a path under neither stays `noexec` and read-only.
pub(crate) fn mount_plan(root: &Path, exec: &[PathBuf], write: &[PathBuf]) -> Vec<MountStep> {
    let decide = |path: &Path, grants: &[PathBuf]| {
        let grant =
            grants.iter().filter(|grant| path.starts_with(grant)).map(|grant| grant.as_os_str().len()).max();
        let deny = path.starts_with(root).then_some(root.as_os_str().len());
        match (grant, deny) {
            (Some(grant), Some(deny)) => grant > deny,
            (grant, _) => grant.is_some(),
        }
    };
    let mut paths: Vec<&Path> = Vec::new();
    for path in exec.iter().chain(write).map(PathBuf::as_path).chain([root]) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths.sort_by_key(|path| (path.as_os_str().len(), *path == root));
    paths
        .into_iter()
        .map(|path| MountStep {
            path: path.to_path_buf(),
            exec: decide(path, exec),
            write: decide(path, write),
        })
        .collect()
}

fn cstring(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("{} holds a NUL byte", path.display()))
}

fn errno(step: &str) -> String {
    format!("{step}: {}", std::io::Error::last_os_error())
}

/// `mount_setattr(path, AT_RECURSIVE)` setting the `set` and clearing the
/// `clr` attributes (`MOUNT_ATTR_NOEXEC`, `MOUNT_ATTR_RDONLY`) on the mount
/// at `path` and every mount beneath it. `Err` says which path failed and
/// why; its `raw_os_error` is the syscall's errno.
fn set_attrs(path: &CString, set: u64, clr: u64) -> Result<(), std::io::Error> {
    let mut attr: libc::mount_attr = unsafe { std::mem::zeroed() };
    attr.attr_set = set;
    attr.attr_clr = clr;
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
        let error = std::io::Error::last_os_error();
        Err(std::io::Error::new(error.kind(), format!("mount_setattr {}: {error}", path.to_string_lossy())))
    }
}

/// Sets one mount step's flags on `path` and beneath it. A mount the parent
/// namespace locked read-only (`MNT_LOCK_READONLY`) cannot be made writable:
/// that step then keeps it read-only, which confines more, rather than fail
/// the whole layer.
fn apply_step(path: &CString, exec: bool, write: bool) -> Result<(), String> {
    let (mut set, mut clr) = (0, 0);
    let mut flag = |on: bool, attr: u64| if on { clr |= attr } else { set |= attr };
    flag(exec, libc::MOUNT_ATTR_NOEXEC);
    flag(write, libc::MOUNT_ATTR_RDONLY);
    match set_attrs(path, set, clr) {
        Err(error) if write && error.kind() == std::io::ErrorKind::PermissionDenied => {
            set_attrs(path, set, clr & !libc::MOUNT_ATTR_RDONLY).map_err(|error| error.to_string())
        }
        result => result.map_err(|error| error.to_string()),
    }
}

/// Enters a new user + mount + network namespace (uid and gid mapped to
/// themselves) in which every mount is `noexec` except under `allowed` and
/// read-only except under `writable`, and `root` is `noexec` and read-only
/// again wherever it lies. The network namespace has only a loopback that is
/// down, so no socket reaches any address. Mounts never propagate out.
fn mount_layer(root: &Path, allowed: &[PathBuf], writable: &[PathBuf]) -> Result<(), String> {
    let plan = mount_plan(root, allowed, writable);
    let plan: Vec<(CString, &MountStep)> =
        plan.iter().map(|step| cstring(&step.path).map(|path| (path, step))).collect::<Result<_, _>>()?;
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let cwd = std::env::current_dir().ok();

    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWNET) } != 0 {
        return Err(errno("unshare(CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET)"));
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
    set_attrs(&slash, libc::MOUNT_ATTR_NOEXEC | libc::MOUNT_ATTR_RDONLY, 0)
        .map_err(|error| error.to_string())?;
    for (path, step) in &plan {
        let bound = unsafe {
            libc::mount(path.as_ptr(), path.as_ptr(), none, libc::MS_BIND | libc::MS_REC, std::ptr::null())
        };
        if bound != 0 {
            return Err(errno(&format!("bind {}", path.to_string_lossy())));
        }
        apply_step(path, step.exec, step.write)?;
    }
    // The working directory still resolves through the mounts it was
    // entered under; re-entering it resolves it through the new binds.
    if let Some(cwd) = cwd {
        let _ = std::env::set_current_dir(cwd);
    }
    Ok(())
}

/// Restricts this process and its descendants with Landlock: exec only
/// under `allowed`; writes (create, remove, write, refer, truncate, device
/// ioctl, connect to a pathname unix socket, each from the ABI that has it)
/// only under `writable` and `/dev`; no TCP bind or connect (ABI >= 4); no
/// abstract unix socket or signal outside the domain (ABI >= 6). Best
/// effort: each right the kernel lacks is dropped. `Err` when the kernel
/// enforces none of it.
fn landlock_layer(allowed: &[PathBuf], writable: &[PathBuf]) -> Result<(), String> {
    use landlock::{
        path_beneath_rules, AccessFs, AccessNet, LandlockStatus, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, Scope, ABI,
    };
    let write = AccessFs::from_write(ABI::V9);
    let write_beneath: Vec<PathBuf> = writable
        .iter()
        .cloned()
        .chain(OS_WRITE.iter().filter_map(|prefix| std::fs::canonicalize(prefix).ok()))
        .collect();
    let status = Ruleset::default()
        .handle_access(write | AccessFs::Execute)
        .and_then(|ruleset| ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp))
        .and_then(|ruleset| ruleset.scope(Scope::AbstractUnixSocket | Scope::Signal))
        .and_then(|ruleset| ruleset.create())
        .and_then(|ruleset| ruleset.add_rules(path_beneath_rules(allowed, AccessFs::Execute)))
        .and_then(|ruleset| ruleset.add_rules(path_beneath_rules(&write_beneath, write)))
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

/// `struct __user_cap_header_struct` for `capset`.
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

/// `struct __user_cap_data_struct`; version 3 takes two of them.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Leaves this process, and everything it execs, with no capabilities:
/// the bounding set and the ambient set cleared, the effective, permitted
/// and inheritable sets zeroed, and `no_new_privs` set so no setuid or
/// file-capability binary grants any back. Runs after both layers (the
/// mount layer needs the namespace's capabilities) and before the exec.
///
/// The bounding set can only be cleared holding `CAP_SETPCAP`; without it
/// (Landlock-only tier, unprivileged user) a failure is accepted only when
/// neither the real nor the effective uid is 0, because only a root exec
/// fills the permitted set from the bounding set.
fn drop_capabilities() -> Result<(), String> {
    let mut bounding_error = None;
    for cap in 0.. {
        let held = unsafe { libc::prctl(libc::PR_CAPBSET_READ, cap as libc::c_ulong, 0, 0, 0) };
        if held < 0 {
            // EINVAL: past the kernel's last capability.
            break;
        }
        if held == 1
            && unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) } != 0
            && bounding_error.is_none()
        {
            bounding_error = Some(errno(&format!("PR_CAPBSET_DROP {cap}")));
        }
    }
    if let Some(error) = bounding_error {
        if unsafe { libc::getuid() == 0 || libc::geteuid() == 0 } {
            return Err(error);
        }
    }
    let clear_ambient = unsafe {
        libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong, 0, 0, 0)
    };
    if clear_ambient != 0 {
        return Err(errno("PR_CAP_AMBIENT_CLEAR_ALL"));
    }
    let header = CapHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let data = [CapData::default(); 2];
    if unsafe { libc::syscall(libc::SYS_capset, &header as *const CapHeader, data.as_ptr()) } != 0 {
        return Err(errno("capset"));
    }
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, 0, 0, 0) } != 0 {
        return Err(errno("PR_SET_NO_NEW_PRIVS"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    /// The probe error text for a trampoline that exited `code`, as
    /// `lsp::probe` words it.
    fn probe_error(code: i32) -> String {
        format!("`--version` exited exit status: {code} (g-mesh sandbox: step failed)")
    }

    #[test]
    fn next_rung_falls_back_from_the_full_tier_by_the_failed_layer() {
        assert_eq!(next_rung(Layers::Full, Some(EXIT_LANDLOCK)), Some(Layers::Mount));
        assert_eq!(next_rung(Layers::Full, Some(EXIT_MOUNT)), Some(Layers::Landlock));
    }

    #[test]
    fn next_rung_ends_the_ladder_on_any_other_failure() {
        for exit in [Some(EXIT_USAGE), Some(EXIT_EXEC), Some(EXIT_CAPS), Some(0), Some(1), None] {
            assert_eq!(next_rung(Layers::Full, exit), None, "full tier, exit {exit:?}");
        }
        for tried in [Layers::Mount, Layers::Landlock] {
            for exit in [Some(EXIT_MOUNT), Some(EXIT_LANDLOCK), Some(EXIT_CAPS), None] {
                assert_eq!(next_rung(tried, exit), None, "{tried:?}, exit {exit:?}");
            }
        }
    }

    #[test]
    fn exit_code_reads_the_status_from_the_probe_error() {
        assert_eq!(exit_code(&probe_error(EXIT_CAPS)), Some(EXIT_CAPS));
        assert_eq!(exit_code(&probe_error(EXIT_MOUNT)), Some(EXIT_MOUNT));
        assert_eq!(exit_code("`--version` exited signal: 9 (SIGKILL)"), None);
        assert_eq!(exit_code("`--version` did not answer within 60s"), None);
    }

    #[test]
    fn failure_names_the_step_of_each_exit_code() {
        for (code, step) in [
            (EXIT_USAGE, "rejected its arguments"),
            (EXIT_MOUNT, "noexec mount namespace"),
            (EXIT_LANDLOCK, "Landlock cannot be applied"),
            (EXIT_EXEC, "could not exec rust-analyzer"),
            (EXIT_CAPS, "could not drop its capabilities"),
        ] {
            let reason = failure(&probe_error(code));
            assert!(reason.contains(step) && reason.contains(&probe_error(code)), "{code}: {reason}");
        }
    }

    #[test]
    fn prefix_over_matches_whole_path_components_only() {
        let allowed = paths(&["/usr", "/home/u/.rustup"]);
        assert_eq!(prefix_over(Path::new("/usr/src/proj"), &allowed), Some(&allowed[0]));
        assert_eq!(prefix_over(Path::new("/usr"), &allowed), Some(&allowed[0]));
        assert_eq!(prefix_over(Path::new("/home/u/.rustup/x"), &allowed), Some(&allowed[1]));
        assert_eq!(prefix_over(Path::new("/usrlocal/proj"), &allowed), None);
        assert_eq!(prefix_over(Path::new("/home/u/.rustup-proj"), &allowed), None);
        assert_eq!(prefix_over(Path::new("/home/u"), &allowed), None);
        assert_eq!(prefix_over(Path::new("/usr/src/proj"), &[]), None);
    }

    #[test]
    fn mount_plan_rebinds_every_prefix_exec_and_the_root_noexec_most_specific_last() {
        let root = Path::new("/home/u/proj");
        let allowed = paths(&["/usr", "/home/u", "/opt/toolchains/stable-x86_64"]);
        let plan = mount_plan(root, &allowed, &[]);

        assert_eq!(plan.len(), allowed.len() + 1, "{plan:?}");
        for prefix in &allowed {
            assert!(
                plan.contains(&MountStep { path: prefix.clone(), exec: true, write: false }),
                "{} is not rebound exec: {plan:?}",
                prefix.display()
            );
        }
        assert!(
            plan.contains(&MountStep { path: root.to_path_buf(), exec: false, write: false }),
            "the root is not made noexec: {plan:?}"
        );
        assert!(
            plan.windows(2).all(|pair| pair[0].path.as_os_str().len() <= pair[1].path.as_os_str().len()),
            "the steps are not ascending by path length: {plan:?}"
        );
        let at = |path: &str| plan.iter().position(|step| step.path == Path::new(path)).unwrap();
        assert!(at("/home/u") < at("/home/u/proj"), "the root's noexec does not follow its prefix: {plan:?}");
    }

    #[test]
    fn mount_plan_makes_a_root_that_is_itself_an_allowed_prefix_noexec_last() {
        let root = Path::new("/srv/proj");
        let plan = mount_plan(root, &paths(&["/srv/proj", "/usr"]), &[]);
        assert_eq!(
            plan.last(),
            Some(&MountStep { path: root.to_path_buf(), exec: false, write: false }),
            "{plan:?}"
        );
    }

    fn policy(root: &str, toolchain: &[&str]) -> SandboxPolicy {
        SandboxPolicy { project_root: PathBuf::from(root), toolchain: paths(toolchain), writable: vec![] }
    }

    fn os(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_trampoline_argv_round_trips_paths_with_spaces_and_server_args_with_dashes() {
        let policy = policy("/tmp/a project/--root", &["/opt/tool chain", "/home/u/-x"]);
        let server_args = ["--log-file", "-v", "--", "--layers", "landlock"];
        for layers in [Layers::Full, Layers::Mount, Layers::Landlock] {
            let mut argv: Vec<OsString> =
                trampoline_args(&policy, layers).unwrap().into_iter().map(OsString::from).collect();
            assert_eq!(argv[0], TRAMPOLINE_ARG);
            argv.push("/opt/tool chain/rust-analyzer".into());
            argv.extend(os(&server_args));

            let invocation = parse(argv.into_iter().skip(1)).unwrap();
            assert_eq!(
                invocation,
                Invocation {
                    layers,
                    root: policy.project_root.clone(),
                    allow: policy.toolchain.clone(),
                    write: policy.writable.clone(),
                    server: PathBuf::from("/opt/tool chain/rust-analyzer"),
                    args: os(&server_args),
                }
            );
        }
    }

    #[test]
    fn parse_refuses_an_incomplete_or_unknown_argv() {
        let cases: [&[&str]; 8] = [
            &["--layers", "mount", "--root", "/r", "/bin/true"],
            &["--layers", "mount", "--root", "/r", "--"],
            &["--layers", "mount", "--root", "/r", "--", "true"],
            &["--layers", "mount,landlock,seccomp", "--root", "/r", "--", "/bin/true"],
            &["--layers", "mount", "--root", "/r", "--net", "off", "--", "/bin/true"],
            &["--root", "/r", "--", "/bin/true"],
            &["--layers", "mount", "--", "/bin/true"],
            &["--layers", "mount", "--root"],
        ];
        for case in cases {
            assert!(parse(os(case)).is_err(), "{case:?} parsed");
        }
    }

    #[test]
    fn trampoline_args_refuses_a_path_that_is_not_utf8() {
        let mut bad = policy("/r", &[]);
        bad.toolchain.push(PathBuf::from(OsString::from_vec(b"/opt/\xff".to_vec())));
        assert!(trampoline_args(&bad, Layers::Full).is_err());
    }

    #[test]
    fn the_trampoline_runs_only_on_its_own_argument_and_rejects_bad_argv_before_any_layer() {
        assert_eq!(trampoline(os(&["g-mesh-plugin-rust"])), None);
        assert_eq!(trampoline(os(&["g-mesh-plugin-rust", "serve", TRAMPOLINE_ARG])), None);
        let usage = trampoline(os(&[
            "g-mesh-plugin-rust",
            TRAMPOLINE_ARG,
            "--layers",
            "none",
            "--root",
            "/r",
            "--",
            "/bin/true",
        ]));
        assert_eq!(usage, Some(EXIT_USAGE));
        let relative = trampoline(os(&[
            "g-mesh-plugin-rust",
            TRAMPOLINE_ARG,
            "--layers",
            "mount",
            "--root",
            "/r",
            "--",
            "true",
        ]));
        assert_eq!(relative, Some(EXIT_USAGE));
    }

    /// A stand-in launcher: it records each `--layers` it is asked for in
    /// `<root>/tried` and exits with the code in `<root>/exit-<layers>`, 0
    /// when there is none; called without the trampoline argument it exits
    /// 0. Written once per user to a fixed path and run once before it is
    /// returned: exec'ing a file another thread's fork still holds open for
    /// writing fails `ETXTBSY`.
    fn fake_launcher() -> &'static Path {
        static LAUNCHER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        LAUNCHER.get_or_init(|| {
            const SCRIPT: &str = "#!/bin/sh\n[ \"$1\" = __g-mesh-sandbox ] || exit 0\n\
                 echo \"$3\" >> \"$5/tried\"\n\
                 code=$(cat \"$5/exit-$3\" 2>/dev/null) || code=0\n\
                 echo \"g-mesh sandbox: fake failure of $3\" >&2\n\
                 exit \"$code\"\n";
            let dir = std::env::temp_dir()
                .join(format!("g-mesh-landlock-ns-fake-launcher-{}", unsafe { libc::getuid() }));
            std::fs::create_dir_all(&dir).unwrap();
            let launcher = dir.join("launcher");
            if std::fs::read_to_string(&launcher).ok().as_deref() != Some(SCRIPT) {
                let staged = dir.join(format!("launcher.{}", std::process::id()));
                std::fs::write(&staged, SCRIPT).unwrap();
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
                std::fs::rename(&staged, &launcher).unwrap();
            }
            for _ in 0..100 {
                match std::process::Command::new(&launcher).status() {
                    Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    result => {
                        assert!(result.as_ref().is_ok_and(|status| status.success()), "{result:?}");
                        return launcher;
                    }
                }
            }
            panic!("{} stayed busy", launcher.display())
        })
    }

    /// A scratch directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("g-mesh-landlock-ns-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("proj")).unwrap();
            Scratch(dir.canonicalize().unwrap())
        }

        fn root(&self) -> PathBuf {
            self.0.join("proj")
        }

        /// Writes the exit code [`fake_launcher`] gives each tier in `exits`.
        fn fail(&self, exits: &[(Layers, i32)]) {
            for (layers, code) in exits {
                std::fs::write(self.root().join(format!("exit-{}", layers.as_arg())), code.to_string())
                    .unwrap();
            }
        }

        fn tried(&self) -> Vec<String> {
            std::fs::read_to_string(self.root().join("tried"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SERVER: &str = "/opt/ra/rust-analyzer";

    fn server_config() -> SemanticConfig {
        let mut config = SemanticConfig::new(Path::new(SERVER));
        config.args = vec!["--log-file".to_string(), "/tmp/a b".to_string()];
        config
    }

    /// `confine` through a launcher that fails each tier in `exits`.
    fn confine_with(
        scratch: &Scratch,
        toolchain: Vec<PathBuf>,
        exits: &[(Layers, i32)],
    ) -> (SandboxOutcome, SemanticConfig, SandboxPolicy) {
        scratch.fail(exits);
        let launcher = fake_launcher();
        let policy = SandboxPolicy { project_root: scratch.root(), toolchain, writable: vec![] };
        let mut config = server_config();
        let outcome = confine(&mut config, &policy, launcher);
        (outcome, config, policy)
    }

    fn assert_rewritten(config: &SemanticConfig, policy: &SandboxPolicy, layers: Layers) {
        let mut expected = trampoline_args(policy, layers).unwrap();
        expected.push(SERVER.to_string());
        expected.extend(server_config().args);
        assert_eq!(config.command, fake_launcher());
        assert_eq!(config.args, expected);
    }

    #[test]
    fn confine_runs_the_full_tier_when_its_preflight_passes() {
        let scratch = Scratch::new("full");
        let (outcome, config, policy) = confine_with(&scratch, vec![], &[]);
        assert_eq!(outcome, SandboxOutcome::Confined { backend: "noexec mounts + Landlock" });
        assert_rewritten(&config, &policy, Layers::Full);
        assert_eq!(scratch.tried(), ["mount,landlock"]);
    }

    #[test]
    fn confine_falls_back_to_the_mount_layer_without_landlock() {
        let scratch = Scratch::new("mount-only");
        let (outcome, config, policy) = confine_with(&scratch, vec![], &[(Layers::Full, EXIT_LANDLOCK)]);
        assert_eq!(outcome, SandboxOutcome::Confined { backend: "noexec mounts" });
        assert_rewritten(&config, &policy, Layers::Mount);
        assert_eq!(scratch.tried(), ["mount,landlock", "mount"]);
    }

    #[test]
    fn confine_falls_back_to_landlock_alone_as_partial_with_the_mount_failure_as_gap() {
        let scratch = Scratch::new("landlock-only");
        let (outcome, config, policy) = confine_with(&scratch, vec![], &[(Layers::Full, EXIT_MOUNT)]);
        let SandboxOutcome::Partial { backend, gap } = outcome else {
            panic!("the Landlock-only tier is not Partial: {outcome:?}")
        };
        assert_eq!(backend, "Landlock");
        assert!(gap.contains("noexec mount namespace cannot be set up"), "{gap}");
        assert!(!gap.contains("exec of project binaries"), "the root lies under no allowed prefix: {gap}");
        assert_rewritten(&config, &policy, Layers::Landlock);
        assert_eq!(scratch.tried(), ["mount,landlock", "landlock"]);
    }

    #[test]
    fn a_landlock_only_gap_names_project_exec_when_the_root_lies_under_an_allowed_prefix() {
        let scratch = Scratch::new("landlock-under");
        let (outcome, _, _) = confine_with(&scratch, vec![scratch.0.clone()], &[(Layers::Full, EXIT_MOUNT)]);
        let SandboxOutcome::Partial { gap, .. } = outcome else {
            panic!("the Landlock-only tier is not Partial: {outcome:?}")
        };
        assert!(
            gap.contains("exec of project binaries is not blocked")
                && gap.contains(&scratch.root().display().to_string())
                && gap.contains(&format!("allowed prefix {}", scratch.0.display())),
            "{gap}"
        );
    }

    #[test]
    fn confine_fails_with_both_reasons_when_neither_layer_works() {
        let scratch = Scratch::new("neither");
        let (outcome, config, _) =
            confine_with(&scratch, vec![], &[(Layers::Full, EXIT_MOUNT), (Layers::Landlock, EXIT_LANDLOCK)]);
        let SandboxOutcome::Failed(reason) = outcome else { panic!("not Failed: {outcome:?}") };
        assert!(
            reason.contains("noexec mount namespace cannot be set up")
                && reason.contains("Landlock cannot be applied"),
            "{reason}"
        );
        assert_eq!(config, server_config(), "a failed confine changed the config");
        assert_eq!(scratch.tried(), ["mount,landlock", "landlock"]);
    }

    #[test]
    fn confine_fails_without_a_fallback_on_a_second_rung_failure() {
        let scratch = Scratch::new("mount-fails");
        let (outcome, config, _) =
            confine_with(&scratch, vec![], &[(Layers::Full, EXIT_LANDLOCK), (Layers::Mount, EXIT_MOUNT)]);
        assert!(matches!(outcome, SandboxOutcome::Failed(_)), "{outcome:?}");
        assert_eq!(config, server_config());
        assert_eq!(scratch.tried(), ["mount,landlock", "mount"]);
    }

    #[test]
    fn confine_fails_at_once_on_a_usage_exec_or_capability_failure() {
        for (code, step) in [
            (EXIT_CAPS, "could not drop its capabilities"),
            (EXIT_USAGE, "rejected its arguments"),
            (EXIT_EXEC, "could not exec rust-analyzer"),
        ] {
            let scratch = Scratch::new("no-fallback");
            let (outcome, config, _) = confine_with(&scratch, vec![], &[(Layers::Full, code)]);
            let SandboxOutcome::Failed(reason) = outcome else {
                panic!("exit {code} is not Failed: {outcome:?}")
            };
            assert!(reason.contains(step), "exit {code}: {reason}");
            assert_eq!(config, server_config(), "exit {code}: a failed confine changed the config");
            assert_eq!(scratch.tried(), ["mount,landlock"], "exit {code} tried a fallback");
        }
    }
}
