//! The OS sandbox rust-analyzer and every process it starts run under.
//!
//! The sandbox decides one thing: what the server's process tree may map as
//! executable code and exec. The toolchain and the OS may be; nothing under
//! the project root may be, even when the root lies inside an allowed
//! prefix. A cargo config `[env]` that sets a dynamic-loader variable, a
//! project binary named as a tool, a project proc-macro dylib: each one is
//! a map or an exec of project code, and each one fails under the sandbox
//! however it was reached
//! ([ADR 0039](../../../docs/adr/0039-rust-server-runs-under-an-os-sandbox.md)).
//!
//! [`SandboxPolicy`] says what is allowed and denied, independent of the OS;
//! [`confine`] rewrites the server's [`SemanticConfig`] so it starts under
//! this OS's backend and reports a [`SandboxOutcome`]. On macOS the backend
//! is Seatbelt (`sandbox-exec`); an OS with no backend leaves the config
//! untouched and says so.

use std::path::{Path, PathBuf};

use g_mesh_plugin_sdk::lsp::SemanticConfig;

use crate::semantic::Toolchain;

/// What the rust-analyzer process tree may map executable and exec,
/// independent of the OS. Every path is canonical (symlinks resolved): a
/// sandbox matches the resolved path of what is mapped or exec'd, so a rule
/// written on an alias would not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Nothing under it is mapped executable or exec'd.
    pub project_root: PathBuf,
    /// Toolchain prefixes the tree may map and exec (sysroot, rustup proxies
    /// dir, server dir, rustc/cargo dirs), deduplicated. OS prefixes are each
    /// backend's own.
    pub toolchain: Vec<PathBuf>,
}

/// What [`confine`] did to the server's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxOutcome {
    /// The config now starts the server, and every child, confined.
    Confined {
        /// The backend that confines it, for log lines (`"seatbelt"`).
        backend: &'static str,
    },
    /// No backend on this OS; the config is untouched.
    NoBackend,
    /// A backend exists but could not confine here; the config is untouched.
    Failed(String),
}

impl SandboxPolicy {
    /// A policy denying `project_root` and allowing `toolchain`, each path
    /// canonicalized. A toolchain prefix that does not exist is dropped (it
    /// holds nothing to map or exec); a root that cannot be canonicalized is
    /// kept as given.
    pub fn new(project_root: &Path, toolchain: impl IntoIterator<Item = PathBuf>) -> Self {
        let project_root = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
        let mut prefixes: Vec<PathBuf> = Vec::new();
        for prefix in toolchain {
            if let Ok(prefix) = std::fs::canonicalize(&prefix) {
                if !prefixes.contains(&prefix) {
                    prefixes.push(prefix);
                }
            }
        }
        SandboxPolicy { project_root, toolchain: prefixes }
    }

    /// The policy for `server` (the resolved rust-analyzer) running on
    /// `root` with `toolchain`: the sysroot, the rustup proxies' directory,
    /// and the directories of the server, `rustc` and `cargo`, which lie
    /// outside the sysroot on an install that is not rustup's.
    pub(crate) fn for_server(root: &Path, toolchain: &Toolchain, server: &Path) -> Self {
        let server = std::fs::canonicalize(server).unwrap_or_else(|_| server.to_path_buf());
        let parent = |path: &Path| path.parent().map(Path::to_path_buf);
        let prefixes = [
            toolchain.sysroot.clone(),
            toolchain.rustup.as_deref().and_then(parent),
            parent(&server),
            toolchain.rustc.as_deref().and_then(parent),
            toolchain.cargo.as_deref().and_then(parent),
        ];
        SandboxPolicy::new(root, prefixes.into_iter().flatten())
    }
}

/// Rewrites `config` (command and args) so the server starts under this OS's
/// sandbox, after a preflight of the wrapped command. On any outcome but
/// [`SandboxOutcome::Confined`] the config is left as it was.
pub fn confine(config: &mut SemanticConfig, policy: &SandboxPolicy) -> SandboxOutcome {
    #[cfg(target_os = "macos")]
    {
        seatbelt::confine(config, policy)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (config, policy);
        SandboxOutcome::NoBackend
    }
}

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

    /// The profile's operations: mapping a file as executable code, and
    /// exec.
    const MAP: &str = "file-map-executable";
    const EXEC: &str = "process-exec";
    const MAP_EXEC: &str = "file-map-executable process-exec";

    /// OS prefixes whose libraries every process maps.
    const OS_MAP: [&str; 3] = ["/System", "/usr/lib", "/Library/Apple"];
    /// OS prefixes whose binaries cargo, rustup and the server may exec.
    const OS_EXEC: [&str; 4] = ["/bin", "/usr/bin", "/usr/libexec", "/System"];
    /// Package-manager prefixes where a toolchain or its libraries may live
    /// (Homebrew on Apple silicon and Intel, MacPorts, nix). Allowed for
    /// both operations; one that does not exist is skipped.
    const PACKAGE_PREFIXES: [&str; 4] = ["/opt/homebrew", "/usr/local", "/opt/local", "/nix/store"];

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
    /// last.
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
        let mut deduped: Vec<(&'static str, PathBuf)> = Vec::new();
        for allow in allows {
            if !deduped.contains(&allow) {
                deduped.push(allow);
            }
        }

        let mut rules: Vec<(bool, &'static str, PathBuf)> =
            deduped.into_iter().map(|(ops, path)| (true, ops, path)).collect();
        rules.push((false, MAP_EXEC, policy.project_root.clone()));
        rules.sort_by_key(|(allow, _, path)| (path.as_os_str().len(), !allow));

        let mut next = 0;
        rules
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
            .collect()
    }

    /// The profile text for `rules`. It names paths only through
    /// parameters, so no path is ever spelled inside SBPL.
    pub(crate) fn profile(rules: &[Rule]) -> String {
        let mut text =
            String::from("(version 1)\n(allow default)\n(deny file-map-executable)\n(deny process-exec)\n");
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
        match lsp::probe(sandbox_exec, wrapped, PROBE_BUDGET) {
            Ok(version) if version == "no version reported" => {
                Err("the confined rust-analyzer printed no version".to_string())
            }
            Ok(_) => Ok(()),
            Err(err) => Err(failure(&format!("{err:#}"))),
        }
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
        let outcome = confine(&mut config, policy);
        (outcome, config)
    }

    fn position(rules: &[Rule], allow: bool, path: &Path) -> usize {
        rules.iter().position(|rule| rule.allow == allow && rule.path == path).unwrap_or_else(|| {
            panic!("no {} rule for {}: {rules:?}", if allow { "allow" } else { "deny" }, path.display())
        })
    }

    /// The root deny is emitted after an allow of a prefix that holds the
    /// root, and before an allow of a prefix the root holds; at equal length
    /// the deny comes last.
    ///
    /// Control: emit the rules in insertion order (drop the sort in
    /// `seatbelt::rules`).
    #[test]
    fn the_root_deny_follows_the_prefixes_that_hold_it_and_precedes_those_it_holds() {
        let policy = SandboxPolicy {
            project_root: PathBuf::from("/usr/local/src/project"),
            toolchain: vec![PathBuf::from("/usr/local"), PathBuf::from("/usr/local/src/project/.rustup")],
        };
        let rules = seatbelt::rules(&policy);
        let root = position(&rules, false, &policy.project_root);
        assert!(position(&rules, true, Path::new("/usr/local")) < root, "{rules:?}");
        assert!(root < position(&rules, true, Path::new("/usr/local/src/project/.rustup")), "{rules:?}");
        assert!(
            rules.windows(2).all(|pair| pair[0].path.as_os_str().len() <= pair[1].path.as_os_str().len()),
            "rules are not in ascending path length: {rules:?}"
        );

        let tie = SandboxPolicy {
            project_root: PathBuf::from("/opt/toolchain"),
            toolchain: vec![PathBuf::from("/opt/toolchain")],
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
        let (outcome, config) = confine_server(&inside_root, &SandboxPolicy::new(&root, [prefix.clone()]));
        assert!(matches!(outcome, SandboxOutcome::Failed(_)), "a server under the root started: {outcome:?}");
        assert_eq!(config, SemanticConfig::new(&inside_root), "a failed confine changed the config");

        let home = scratch.0.join("home");
        let rustup = home.join(".rustup");
        let in_prefix = fake_server(&rustup);
        let (outcome, _) = confine_server(&in_prefix, &SandboxPolicy::new(&home, [rustup.clone()]));
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

        let policy = SandboxPolicy::for_server(&scratch.0, &toolchain, &server);
        let (outcome, config) = confine_server(&server, &policy);
        assert_eq!(outcome, SandboxOutcome::Confined { backend: "seatbelt" }, "{policy:?}");
        assert_eq!(config.command, Path::new(seatbelt::SANDBOX_EXEC));
        assert_eq!(config.args.last().map(String::as_str), server.to_str(), "{:?}", config.args);

        let without_sysroot = Toolchain { sysroot: None, ..toolchain };
        let policy = SandboxPolicy::for_server(&scratch.0, &without_sysroot, &server);
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

        let policy = SandboxPolicy::new(&root, [prefix]);
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
}
