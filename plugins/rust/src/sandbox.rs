//! The OS sandbox rust-analyzer and every process it starts run under.
//!
//! The sandbox decides three things. What the server's process tree may map
//! as executable code and exec: the toolchain and the OS may be; nothing
//! under the project root may be, even when the root lies inside an allowed
//! prefix. A cargo config `[env]` that sets a dynamic-loader variable, a
//! project binary named as a tool, a project proc-macro dylib: each one is
//! a map or an exec of project code, and each one fails under the sandbox
//! however it was reached
//! ([ADR 0039](../../../docs/adr/0039-rust-server-runs-under-an-os-sandbox.md)).
//! What it may write: only the server's private temp dir and the cargo
//! caches (`$CARGO_HOME/registry`, `$CARGO_HOME/git`), never anything under
//! the project root. And the network: the tree opens no connection, so
//! offline cargo is enforced by the OS, not only by configuration
//! ([ADR 0040](../../../docs/adr/0040-rust-sandbox-denies-network-and-writes.md)).
//!
//! [`SandboxPolicy`] says what is allowed and denied, independent of the OS;
//! [`confine`] rewrites the server's [`SemanticConfig`] so it starts under
//! this OS's backend and reports a [`SandboxOutcome`]. On macOS the backend
//! is Seatbelt (`sandbox-exec`). On Linux it is the plugin binary itself as
//! a trampoline (`landlock_ns`): a `noexec` mount namespace plus Landlock,
//! or whichever of the two this host supports. An OS with no backend leaves
//! the config untouched and says so.

use std::path::{Path, PathBuf};

use g_mesh_plugin_sdk::lsp::SemanticConfig;

use crate::semantic::Toolchain;

/// What the rust-analyzer process tree may map executable, exec and write,
/// independent of the OS. Nothing in the tree may open a network
/// connection. Every path is canonical (symlinks resolved): a
/// sandbox matches the resolved path of what is mapped or exec'd, so a rule
/// written on an alias would not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Nothing under it is mapped executable, exec'd or written, even inside
    /// a toolchain or writable prefix.
    pub project_root: PathBuf,
    /// Toolchain prefixes the tree may map and exec (sysroot, rustup proxies
    /// dir, server dir, rustc/cargo dirs), deduplicated. OS prefixes are each
    /// backend's own.
    pub toolchain: Vec<PathBuf>,
    /// Prefixes the tree may write (the server's private temp dir and the
    /// cargo caches), deduplicated. A writable prefix inside the root stays
    /// writable; the root inside one stays read-only. OS write prefixes
    /// (`/dev`) are each backend's own.
    pub writable: Vec<PathBuf>,
}

/// What [`confine`] did to the server's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxOutcome {
    /// The config now starts the server, and every child, confined.
    Confined {
        /// The backend that confines it, for log lines (`"seatbelt"`).
        backend: &'static str,
    },
    /// The config now starts the server under a backend that blocks less
    /// than the policy asks: `gap` says what is not blocked and why.
    Partial {
        /// The backend that partly confines it, for log lines.
        backend: &'static str,
        /// What stays open and why, for log lines.
        gap: String,
    },
    /// No backend on this OS; the config is untouched.
    NoBackend,
    /// A backend exists but could not confine here; the config is untouched.
    Failed(String),
}

impl SandboxPolicy {
    /// A policy denying `project_root`, allowing map and exec under
    /// `toolchain` and writes under `writable`, each path canonicalized. A
    /// prefix that does not exist is dropped (it holds nothing to map, exec
    /// or keep); a root that cannot be canonicalized is kept as given.
    pub fn new(
        project_root: &Path,
        toolchain: impl IntoIterator<Item = PathBuf>,
        writable: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let project_root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
        SandboxPolicy {
            project_root,
            toolchain: canonical_prefixes(toolchain),
            writable: canonical_prefixes(writable),
        }
    }

    /// The policy for `server` (the resolved rust-analyzer) running on
    /// `root` with `toolchain`: the sysroot, the rustup proxies' directory,
    /// and the directories of the server, `rustc` and `cargo`, which lie
    /// outside the sysroot on an install that is not rustup's. The tree may
    /// write `temp` (the server's private `TMPDIR`) and `cargo_home`'s
    /// `registry/` and `git/` (extracting an already cached dependency), never
    /// `cargo_home` itself, whose `bin/` and config files reach the user's own
    /// later cargo runs.
    pub(crate) fn for_server(
        root: &Path,
        toolchain: &Toolchain,
        server: &Path,
        cargo_home: Option<&Path>,
        temp: Option<&Path>,
    ) -> Self {
        let server = std::fs::canonicalize(server).unwrap_or_else(|_| server.to_path_buf());
        let parent = |path: &Path| path.parent().map(Path::to_path_buf);
        let prefixes = [
            toolchain.sysroot.clone(),
            toolchain.rustup.as_deref().and_then(parent),
            parent(&server),
            toolchain.rustc.as_deref().and_then(parent),
            toolchain.cargo.as_deref().and_then(parent),
        ];
        let writable = [
            temp.map(Path::to_path_buf),
            cargo_home.map(|home| home.join("registry")),
            cargo_home.map(|home| home.join("git")),
        ];
        SandboxPolicy::new(root, prefixes.into_iter().flatten(), writable.into_iter().flatten())
    }
}

/// `prefixes` canonicalized, the missing ones dropped, deduplicated in order.
fn canonical_prefixes(prefixes: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut canonical: Vec<PathBuf> = Vec::new();
    for prefix in prefixes {
        if let Ok(prefix) = std::fs::canonicalize(&prefix) {
            if !canonical.contains(&prefix) {
                canonical.push(prefix);
            }
        }
    }
    canonical
}

/// Rewrites `config` (command and args) so the server starts under this OS's
/// sandbox, after a preflight of the wrapped command. On any outcome but
/// [`SandboxOutcome::Confined`] and [`SandboxOutcome::Partial`] the config is
/// left as it was. `launcher` is the binary that runs the Linux trampoline
/// (`/proc/self/exe` in the plugin); other backends ignore it.
pub fn confine(config: &mut SemanticConfig, policy: &SandboxPolicy, launcher: &Path) -> SandboxOutcome {
    #[cfg(target_os = "macos")]
    {
        let _ = launcher;
        seatbelt::confine(config, policy)
    }
    #[cfg(target_os = "linux")]
    {
        landlock_ns::confine(config, policy, launcher)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (config, policy, launcher);
        SandboxOutcome::NoBackend
    }
}

#[cfg(target_os = "linux")]
pub use landlock_ns::{landlock_abi, trampoline};

/// The macOS backend: `sandbox-exec` with a profile generated from the
/// policy. `sandbox-exec` execs the server in place, so the spawned pid is
/// the server's and the bridge's kill/wait need nothing new.
#[cfg(target_os = "macos")]
pub(crate) mod seatbelt {
    use std::path::{Path, PathBuf};

    use g_mesh_plugin_sdk::lsp::{self, SemanticConfig, PROBE_BUDGET};

    use super::{SandboxOutcome, SandboxPolicy};

    /// Absolute and SIP-protected: never a `PATH` lookup.
    pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    /// The backend's name in [`SandboxOutcome::Confined`].
    pub(crate) const BACKEND: &str = "seatbelt";

    /// The profile's operations: mapping a file as executable code, exec,
    /// and every kind of write (create, data, unlink, rename, mode, xattr,
    /// times).
    const MAP: &str = "file-map-executable";
    const EXEC: &str = "process-exec";
    const MAP_EXEC: &str = "file-map-executable process-exec";
    const WRITE: &str = "file-write*";
    const MAP_EXEC_WRITE: &str = "file-map-executable process-exec file-write*";

    /// OS prefixes whose libraries every process maps.
    const OS_MAP: [&str; 3] = ["/System", "/usr/lib", "/Library/Apple"];
    /// OS prefixes whose binaries cargo, rustup and the server may exec.
    const OS_EXEC: [&str; 4] = ["/bin", "/usr/bin", "/usr/libexec", "/System"];
    /// Package-manager prefixes where a toolchain or its libraries may live
    /// (Homebrew on Apple silicon and Intel, MacPorts, nix). Allowed for
    /// both operations; one that does not exist is skipped.
    const PACKAGE_PREFIXES: [&str; 4] = ["/opt/homebrew", "/usr/local", "/opt/local", "/nix/store"];
    /// OS prefixes every process may write: `/dev/null`, `/dev/tty`,
    /// `/dev/dtracehelper`.
    const OS_WRITE: [&str; 1] = ["/dev"];

    /// One rule of the profile: `allow` or `deny` of `ops` under `path`,
    /// which reaches the profile as the parameter `param`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Rule {
        pub(crate) allow: bool,
        pub(crate) ops: &'static str,
        pub(crate) path: PathBuf,
        pub(crate) param: String,
    }

    /// The profile's rules for `policy`, in the order they are emitted.
    ///
    /// Seatbelt applies the last matching rule whatever its specificity, so
    /// the rules are emitted in ascending path length: the most specific
    /// rule comes last and wins. The root deny therefore covers a project
    /// inside an allowed prefix, and a toolchain prefix inside the project
    /// root (indexing `$HOME`, which holds `~/.rustup`) stays allowed. At
    /// equal length (the root is itself an allowed prefix) the deny comes
    /// last. Write allows take part in the same ordering: the root deny also
    /// denies writes, so a root inside a writable prefix stays read-only and
    /// a writable prefix inside the root (`~/.cargo/registry` when indexing
    /// `$HOME`) stays writable.
    ///
    /// A writable prefix is never executable: after every length-ordered
    /// rule, each writable prefix gets a deny of map and exec, so it wins over
    /// a broader prefix that allows them (`/usr/local` holding a
    /// `CARGO_HOME` of `/usr/local/cargo`) and over a longer toolchain allow
    /// inside it. Nothing the tree writes can then be run or mapped as code.
    pub(crate) fn rules(policy: &SandboxPolicy) -> Vec<Rule> {
        let mut allows: Vec<(&'static str, PathBuf)> = Vec::new();
        for prefix in &policy.toolchain {
            allows.push((MAP_EXEC, prefix.clone()));
        }
        for prefix in PACKAGE_PREFIXES {
            if let Ok(prefix) = std::fs::canonicalize(prefix) {
                allows.push((MAP_EXEC, prefix));
            }
        }
        allows.extend(OS_MAP.iter().map(|path| (MAP, PathBuf::from(path))));
        allows.extend(OS_EXEC.iter().map(|path| (EXEC, PathBuf::from(path))));
        allows.extend(policy.writable.iter().map(|prefix| (WRITE, prefix.clone())));
        allows.extend(OS_WRITE.iter().map(|path| (WRITE, PathBuf::from(path))));
        let mut deduped: Vec<(&'static str, PathBuf)> = Vec::new();
        for allow in allows {
            if !deduped.contains(&allow) {
                deduped.push(allow);
            }
        }

        let mut rules: Vec<(bool, &'static str, PathBuf)> =
            deduped.into_iter().map(|(ops, path)| (true, ops, path)).collect();
        rules.push((false, MAP_EXEC_WRITE, policy.project_root.clone()));
        rules.sort_by_key(|(allow, _, path)| (path.as_os_str().len(), !allow));

        let mut next = 0;
        let mut emitted: Vec<Rule> = rules
            .into_iter()
            .map(|(allow, ops, path)| {
                let param = if allow {
                    next += 1;
                    format!("ALLOW_{}", next - 1)
                } else {
                    "ROOT".to_string()
                };
                Rule { allow, ops, path, param }
            })
            .collect();
        emitted.extend(policy.writable.iter().enumerate().map(|(n, prefix)| Rule {
            allow: false,
            ops: MAP_EXEC,
            path: prefix.clone(),
            param: format!("NOEXEC_{n}"),
        }));
        emitted
    }

    /// The profile text for `rules`: map, exec and write denied by default,
    /// and every network operation (outbound, inbound, bind) denied. It names
    /// paths only through parameters, so no path is ever spelled inside SBPL.
    pub(crate) fn profile(rules: &[Rule]) -> String {
        let mut text = String::from(
            "(version 1)\n(allow default)\n(deny file-map-executable)\n(deny process-exec)\n\
             (deny file-write*)\n(deny network*)\n",
        );
        for rule in rules {
            let verb = if rule.allow { "allow" } else { "deny" };
            text.push_str(&format!("({verb} {} (subpath (param \"{}\")))\n", rule.ops, rule.param));
        }
        text
    }

    /// `sandbox-exec`'s argv up to and including `--`: the profile and a
    /// `-D NAME=path` binding for every rule. `Err` names a path that is not
    /// UTF-8, which no parameter can carry.
    pub(crate) fn wrapper_args(policy: &SandboxPolicy) -> Result<Vec<String>, String> {
        let rules = rules(policy);
        let mut args = vec!["-p".to_string(), profile(&rules)];
        for rule in &rules {
            let path = rule.path.to_str().ok_or_else(|| format!("{} is not UTF-8", rule.path.display()))?;
            args.push("-D".to_string());
            args.push(format!("{}={path}", rule.param));
        }
        args.push("--".to_string());
        Ok(args)
    }

    /// See [`super::confine`].
    pub(crate) fn confine(config: &mut SemanticConfig, policy: &SandboxPolicy) -> SandboxOutcome {
        let sandbox_exec = Path::new(SANDBOX_EXEC);
        if !sandbox_exec.exists() {
            return SandboxOutcome::Failed(format!("{SANDBOX_EXEC} not found"));
        }
        let mut wrapped = match wrapper_args(policy) {
            Ok(args) => args,
            Err(reason) => {
                return SandboxOutcome::Failed(format!("cannot pass a path to Seatbelt: {reason}"))
            }
        };
        wrapped.push(config.command.to_string_lossy().into_owned());
        if let Err(failure) = preflight(sandbox_exec, &wrapped) {
            return SandboxOutcome::Failed(failure);
        }
        wrapped.append(&mut config.args);
        config.command = sandbox_exec.to_path_buf();
        config.args = wrapped;
        SandboxOutcome::Confined { backend: BACKEND }
    }

    /// Runs the wrapped server with `--version` once, which proves in one
    /// exec that `sandbox-exec` runs, the profile parses with every
    /// parameter bound, and the server may be exec'd and map its own
    /// libraries. `Err` is the reason the sandbox cannot be applied.
    pub(crate) fn preflight(sandbox_exec: &Path, wrapped: &[String]) -> Result<(), String> {
        lsp::probe(sandbox_exec, wrapped, PROBE_BUDGET)
            .map(|_| ())
            .map_err(|err| failure(&format!("{err:#}")))
    }

    /// The reason for a failed preflight, from the probe's error text, which
    /// carries `sandbox-exec`'s exit status: 65 is a profile it rejected,
    /// 71 an exec it refused.
    pub(crate) fn failure(detail: &str) -> String {
        if detail.contains("exit status: 65") {
            format!("Seatbelt profile rejected: {detail}")
        } else if detail.contains("exit status: 71") {
            format!("Seatbelt denied starting rust-analyzer: {detail}")
        } else {
            format!("the confined rust-analyzer did not start: {detail}")
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod landlock_ns;

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::path::{Path, PathBuf};

    use g_mesh_plugin_sdk::lsp::SemanticConfig;

    use super::seatbelt::{self, Rule};
    use super::{confine, SandboxOutcome, SandboxPolicy};
    use crate::semantic::Toolchain;

    /// A directory that removes itself, at `base`/`name`, unique per process
    /// and per call.
    struct Scratch(PathBuf);

    impl Scratch {
        fn under(base: &Path, name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = base.join(format!("g-mesh-rs-sandbox-{name}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        /// Under the temp dir, canonicalized.
        fn new(name: &str) -> Self {
            let scratch = Scratch::under(&std::env::temp_dir(), name);
            let canonical = scratch.0.canonicalize().unwrap();
            Scratch(canonical)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A stand-in server at `dir/rust-analyzer`: a `/bin/sh` script that
    /// answers `--version`. Under Seatbelt its exec is checked against its
    /// own path, so where it lies decides whether the preflight passes.
    fn fake_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let server = dir.join("rust-analyzer");
        std::fs::write(&server, "#!/bin/sh\necho \"rust-analyzer 0.0.0-fake\"\n").unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        server
    }

    /// `confine` of a config that starts `server`, under `policy`.
    fn confine_server(server: &Path, policy: &SandboxPolicy) -> (SandboxOutcome, SemanticConfig) {
        let mut config = SemanticConfig::new(server);
        let outcome = confine(&mut config, policy, Path::new("/proc/self/exe"));
        (outcome, config)
    }

    fn position(rules: &[Rule], allow: bool, path: &Path) -> usize {
        rules.iter().position(|rule| rule.allow == allow && rule.path == path).unwrap_or_else(|| {
            panic!("no {} rule for {}: {rules:?}", if allow { "allow" } else { "deny" }, path.display())
        })
    }

    /// The rules that take part in the length ordering: every rule but the
    /// trailing map/exec denies of the writable prefixes.
    fn ordered(rules: &[Rule]) -> Vec<Rule> {
        rules.iter().filter(|rule| !rule.param.starts_with("NOEXEC_")).cloned().collect()
    }

    /// The root deny is emitted after an allow of a prefix that holds the
    /// root, and before an allow of a prefix the root holds; at equal length
    /// the deny comes last. Writable prefixes take part in the same order,
    /// and the root deny denies writes too: a root inside a writable prefix
    /// stays read-only, a writable prefix inside the root stays writable.
    ///
    /// Control: emit the rules in insertion order (drop the sort in
    /// `seatbelt::rules`).
    #[test]
    fn the_root_deny_follows_the_prefixes_that_hold_it_and_precedes_those_it_holds() {
        let policy = SandboxPolicy {
            project_root: PathBuf::from("/usr/local/src/project"),
            toolchain: vec![PathBuf::from("/usr/local"), PathBuf::from("/usr/local/src/project/.rustup")],
            writable: vec![
                PathBuf::from("/usr/local/src"),
                PathBuf::from("/usr/local/src/project/.cargo/git"),
            ],
        };
        let rules = ordered(&seatbelt::rules(&policy));
        let root = position(&rules, false, &policy.project_root);
        assert!(position(&rules, true, Path::new("/usr/local")) < root, "{rules:?}");
        assert!(root < position(&rules, true, Path::new("/usr/local/src/project/.rustup")), "{rules:?}");
        assert!(position(&rules, true, Path::new("/usr/local/src")) < root, "{rules:?}");
        assert!(root < position(&rules, true, Path::new("/usr/local/src/project/.cargo/git")), "{rules:?}");
        assert!(rules[root].ops.contains("file-write*"), "the root deny does not deny writes: {rules:?}");
        assert!(
            rules.windows(2).all(|pair| pair[0].path.as_os_str().len() <= pair[1].path.as_os_str().len()),
            "rules are not in ascending path length: {rules:?}"
        );

        let tie = SandboxPolicy {
            project_root: PathBuf::from("/opt/toolchain"),
            toolchain: vec![PathBuf::from("/opt/toolchain")],
            writable: vec![],
        };
        let rules = seatbelt::rules(&tie);
        assert!(
            position(&rules, true, &tie.project_root) < position(&rules, false, &tie.project_root),
            "{rules:?}"
        );
    }

    /// The ordering holds where it matters, in Seatbelt itself: a server
    /// inside the root is refused even though an allowed prefix holds the
    /// root, and a server in an allowed prefix inside the root starts (an
    /// indexed `$HOME` holding `~/.rustup`).
    ///
    /// Control: emit the rules in insertion order (drop the sort in
    /// `seatbelt::rules`).
    #[test]
    fn seatbelt_denies_the_root_inside_a_prefix_and_allows_a_prefix_inside_the_root() {
        let scratch = Scratch::new("order");
        let prefix = scratch.0.join("toolchain");
        let root = prefix.join("project");

        let inside_root = fake_server(&root);
        let (outcome, config) =
            confine_server(&inside_root, &SandboxPolicy::new(&root, [prefix.clone()], []));
        assert!(matches!(outcome, SandboxOutcome::Failed(_)), "a server under the root started: {outcome:?}");
        assert_eq!(config, SemanticConfig::new(&inside_root), "a failed confine changed the config");

        let home = scratch.0.join("home");
        let rustup = home.join(".rustup");
        let in_prefix = fake_server(&rustup);
        let (outcome, _) = confine_server(&in_prefix, &SandboxPolicy::new(&home, [rustup.clone()], []));
        assert_eq!(outcome, SandboxOutcome::Confined { backend: "seatbelt" });
    }

    /// The machine's real rust-analyzer starts confined under the policy
    /// `engine` builds, and the config then starts `sandbox-exec` with the
    /// server last; without the sysroot in the policy the same preflight
    /// fails. Skips on a machine with no rust-analyzer.
    ///
    /// Control: drop the sysroot from `SandboxPolicy::for_server`.
    #[test]
    fn the_real_server_starts_confined_and_needs_its_sysroot() {
        let toolchain = Toolchain::detect();
        let Some(server) = toolchain.rust_analyzer.clone() else {
            println!("SKIPPED: no rust-analyzer found");
            return;
        };
        let scratch = Scratch::new("toolchain");

        let policy = SandboxPolicy::for_server(&scratch.0, &toolchain, &server, None, None);
        let (outcome, config) = confine_server(&server, &policy);
        assert_eq!(outcome, SandboxOutcome::Confined { backend: "seatbelt" }, "{policy:?}");
        assert_eq!(config.command, Path::new(seatbelt::SANDBOX_EXEC));
        assert_eq!(config.args.last().map(String::as_str), server.to_str(), "{:?}", config.args);

        let without_sysroot = Toolchain { sysroot: None, ..toolchain };
        let policy = SandboxPolicy::for_server(&scratch.0, &without_sysroot, &server, None, None);
        let (outcome, config) = confine_server(&server, &policy);
        assert!(
            matches!(outcome, SandboxOutcome::Failed(_)),
            "sanity: the server started without its sysroot allowed, so the sysroot is not what lets it start: \
             {outcome:?} under {policy:?}"
        );
        assert_eq!(config.command, server);
    }

    /// A root given through a symlink (`/tmp` is one on macOS) reaches the
    /// profile as its canonical path, which is what Seatbelt matches, so a
    /// server under it is refused even when named through the alias.
    ///
    /// Control: skip `canonicalize` on the project root in
    /// `SandboxPolicy::new`.
    #[test]
    fn a_root_given_through_a_symlink_is_denied_by_its_canonical_path() {
        let alias = Path::new("/tmp");
        let scratch = Scratch::under(alias, "alias");
        let canonical = scratch.0.canonicalize().unwrap();
        assert_ne!(canonical, scratch.0, "sanity: {} is not a symlink here", alias.display());
        let prefix = scratch.0.join("toolchain");
        let root = prefix.join("project");
        let server = fake_server(&root);

        let policy = SandboxPolicy::new(&root, [prefix], []);
        let canonical_root = canonical.join("toolchain/project");
        assert_eq!(policy.project_root, canonical_root);
        let args = seatbelt::wrapper_args(&policy).unwrap();
        assert!(args.contains(&format!("ROOT={}", canonical_root.display())), "{args:?}");

        let (outcome, _) = confine_server(&server, &policy);
        assert!(
            matches!(outcome, SandboxOutcome::Failed(_)),
            "a server under the root, named through {}, started: {outcome:?}",
            alias.display()
        );
    }

    /// Every writable prefix gets a deny of map and exec after every other
    /// rule, so it wins over a broader allowed prefix holding it
    /// (`/usr/local` holding a `CARGO_HOME` of `/usr/local/cargo`) and over a
    /// longer toolchain allow inside it, while the last write rule over it is
    /// still an allow.
    ///
    /// Control: drop the trailing `NOEXEC_` denies from `seatbelt::rules`.
    #[test]
    fn every_writable_prefix_is_denied_map_and_exec_after_every_other_rule() {
        let registry = PathBuf::from("/usr/local/cargo/registry");
        let policy = SandboxPolicy {
            project_root: PathBuf::from("/src/project"),
            toolchain: vec![PathBuf::from("/usr/local"), registry.join("src/tool")],
            writable: vec![registry.clone(), PathBuf::from("/private/tmp/g-mesh-rust-1-0")],
        };
        let rules = seatbelt::rules(&policy);
        let last_allow = rules.iter().rposition(|rule| rule.allow).unwrap();
        for prefix in &policy.writable {
            let deny = rules
                .iter()
                .rposition(|rule| !rule.allow && rule.path == *prefix)
                .unwrap_or_else(|| panic!("no deny for the writable {}: {rules:?}", prefix.display()));
            let ops = rules[deny].ops;
            assert!(
                ops.contains("file-map-executable")
                    && ops.contains("process-exec")
                    && !ops.contains("file-write"),
                "{}: {ops}",
                prefix.display()
            );
            assert!(deny > last_allow, "the deny of {} precedes an allow: {rules:?}", prefix.display());
            let last_write = rules
                .iter()
                .rposition(|rule| rule.ops.contains("file-write") && prefix.starts_with(&rule.path))
                .unwrap();
            assert!(rules[last_write].allow, "{} is not writable: {rules:?}", prefix.display());
        }
    }

    /// A stand-in server at `dir/rust-analyzer` that answers `--version`
    /// and otherwise runs each argument as a step, printing one line per
    /// step: `write=<file>` creates the file, `connect=<port>` opens a TCP
    /// connection to `127.0.0.1:<port>` with `bash`'s `/dev/tcp`, `run=<prog>`
    /// runs a program and prints its exit code.
    fn probe_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        const SCRIPT: &str = "#!/bin/sh\n\
            if [ \"$1\" = --version ]; then echo \"rust-analyzer 0.0.0-probe\"; exit 0; fi\n\
            for step in \"$@\"; do\n\
              case \"$step\" in\n\
                write=*) f=\"${step#write=}\"; if (: > \"$f\") 2>/dev/null; then echo \"wrote $f\"; \
                         else echo \"refused $f\"; fi ;;\n\
                connect=*) if /bin/bash -c \"exec 3<>/dev/tcp/127.0.0.1/${step#connect=}\" 2>/dev/null; \
                           then echo connected; else echo \"not connected\"; fi ;;\n\
                run=*) \"${step#run=}\" >/dev/null 2>&1; echo \"run ${step#run=} rc=$?\" ;;\n\
              esac\n\
            done\n";
        std::fs::create_dir_all(dir).unwrap();
        let server = dir.join("rust-analyzer");
        std::fs::write(&server, SCRIPT).unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        server
    }

    /// The probe's stdout for `steps`, run unconfined (`policy` `None`) or
    /// confined under `policy`, which must confine it.
    fn probe(server: &Path, policy: Option<&SandboxPolicy>, steps: &[String]) -> String {
        let config = match policy {
            None => SemanticConfig::new(server),
            Some(policy) => {
                let (outcome, config) = confine_server(server, policy);
                assert_eq!(outcome, SandboxOutcome::Confined { backend: "seatbelt" }, "{policy:?}");
                config
            }
        };
        let output =
            std::process::Command::new(&config.command).args(&config.args).args(steps).output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "the probe failed ({}): {stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    fn step(kind: &str, path: &Path) -> String {
        format!("{kind}={}", path.display())
    }

    /// A script at `path` that exits 0.
    fn exits_zero(path: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_path_buf()
    }

    /// Confined, the tree writes under a writable prefix and to `/dev/null`,
    /// and nowhere else: not under the root, which lies inside the writable
    /// prefix, and not in a directory no prefix lists.
    ///
    /// Controls: drop `(deny file-write*)` from `seatbelt::profile`
    /// (`out/MARK` written); root deny back to map and exec only in
    /// `seatbelt::rules` (`w/proj/MARK` written).
    #[test]
    fn seatbelt_writes_only_under_writable_prefixes_and_never_under_the_root() {
        let scratch = Scratch::new("writes");
        let writable = scratch.0.join("w");
        let root = writable.join("proj");
        let out = scratch.0.join("out");
        for dir in [&root, &out] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let server = probe_server(&scratch.0.join("tool"));

        let sanity = out.join("SANITY");
        probe(&server, None, &[step("write", &sanity)]);
        assert!(sanity.is_file(), "sanity: the probe writes nothing unconfined");

        let policy = SandboxPolicy::new(&root, [scratch.0.join("tool")], [writable.clone()]);
        let (ok, in_root, outside) = (writable.join("ok"), root.join("MARK"), out.join("MARK"));
        let steps =
            [step("write", &ok), step("write", &in_root), step("write", &outside), "write=/dev/null".into()];
        let stdout = probe(&server, Some(&policy), &steps);
        assert!(ok.is_file(), "the writable prefix was not writable: {stdout}");
        assert!(!in_root.exists(), "a confined process wrote under the root: {stdout}");
        assert!(!outside.exists(), "a confined process wrote outside every writable prefix: {stdout}");
        assert!(stdout.contains("wrote /dev/null"), "{stdout}");
    }

    /// Confined, the tree opens no network connection, not even to a
    /// listener on loopback that the same probe reaches unconfined.
    ///
    /// Control: drop `(deny network*)` from `seatbelt::profile`.
    #[test]
    fn seatbelt_refuses_every_network_connection() {
        let scratch = Scratch::new("network");
        let server = probe_server(&scratch.0.join("tool"));
        let sink = Sink::start();
        let connect = vec![format!("connect={}", sink.port)];

        let stdout = probe(&server, None, &connect);
        assert!(
            stdout.contains("connected") && sink.settled() == 1,
            "sanity: the probe does not connect: {stdout}"
        );

        let policy = SandboxPolicy::new(&scratch.0.join("proj"), [scratch.0.join("tool")], []);
        let stdout = probe(&server, Some(&policy), &connect);
        assert_eq!(sink.settled(), 1, "a confined process connected: {stdout}");
        assert!(stdout.contains("not connected"), "{stdout}");
    }

    /// A writable prefix inside an allowed prefix is not executable, while
    /// the rest of the allowed prefix is, and the writable prefix stays
    /// writable.
    ///
    /// Control: drop the trailing `NOEXEC_` denies from `seatbelt::rules`.
    #[test]
    fn seatbelt_execs_nothing_under_a_writable_prefix_inside_an_allowed_prefix() {
        let scratch = Scratch::new("noexec");
        let tool = scratch.0.join("tool");
        let cache = tool.join("cache");
        let server = probe_server(&tool);
        let in_tool = exits_zero(&tool.join("run.sh"));
        let in_cache = exits_zero(&cache.join("run.sh"));
        let runs = [step("run", &in_tool), step("run", &in_cache)];

        let stdout = probe(&server, None, &runs);
        assert_eq!(stdout.matches("rc=0").count(), 2, "sanity: the scripts do not run unconfined: {stdout}");

        let policy = SandboxPolicy::new(&scratch.0.join("proj"), [tool.clone()], [cache.clone()]);
        let written = cache.join("ok");
        let mut steps = runs.to_vec();
        steps.push(step("write", &written));
        let stdout = probe(&server, Some(&policy), &steps);
        assert!(stdout.contains(&format!("run {} rc=0", in_tool.display())), "{stdout}");
        assert!(
            !stdout.contains(&format!("run {} rc=0", in_cache.display())),
            "a confined process exec'd a file under a writable prefix: {stdout}"
        );
        assert!(written.is_file(), "the writable prefix inside an allowed prefix is not writable: {stdout}");
    }

    /// The writable set is the temp dir and `CARGO_HOME`'s `registry/` and
    /// `git/`, canonical, in that order; never `CARGO_HOME` itself or its
    /// `bin/`, and a missing cache directory is dropped.
    #[test]
    fn for_server_allows_writes_only_to_the_temp_dir_and_cargo_caches() {
        let scratch = Scratch::new("writable");
        let home = scratch.0.join("cargo-home");
        let temp = scratch.0.join("tmp");
        for dir in [home.join("registry"), home.join("git"), home.join("bin"), temp.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let root = scratch.0.join("proj");
        let server = scratch.0.join("ra/rust-analyzer");
        let policy = |home: &Path| {
            SandboxPolicy::for_server(&root, &Toolchain::default(), &server, Some(home), Some(&temp))
        };

        let all = policy(&home);
        assert_eq!(all.writable, vec![temp.clone(), home.join("registry"), home.join("git")]);
        assert!(!all.writable.contains(&home) && !all.writable.contains(&home.join("bin")), "{all:?}");

        std::fs::remove_dir(home.join("git")).unwrap();
        assert_eq!(policy(&home).writable, vec![temp.clone(), home.join("registry")]);
    }

    /// A TCP listener on loopback that counts the connections it accepts.
    /// [`Sink::settled`] makes its own connection last, so every connection
    /// made before the call has been counted when it returns.
    struct Sink {
        port: u16,
        real: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        sentinels: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Sink {
        fn start() -> Self {
            use std::io::Read;
            use std::sync::atomic::Ordering;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let real = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let sentinels = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (counted, marked) = (real.clone(), sentinels.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                    let mut first = [0u8; 1];
                    if stream.read_exact(&mut first).is_ok() && first == *b"S" {
                        marked.fetch_add(1, Ordering::SeqCst);
                    } else {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
            Sink { port, real, sentinels }
        }

        /// The connections accepted so far, the sink's own excluded. The
        /// listener accepts in arrival order, so once the sentinel made here
        /// is counted every earlier connection is too.
        fn settled(&self) -> usize {
            use std::io::Write;
            use std::sync::atomic::Ordering;
            let target = self.sentinels.load(Ordering::SeqCst) + 1;
            let mut sentinel = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
            sentinel.write_all(b"S").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while self.sentinels.load(Ordering::SeqCst) < target {
                assert!(std::time::Instant::now() < deadline, "the sink never accepted its own connection");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            self.real.load(Ordering::SeqCst)
        }
    }
}
