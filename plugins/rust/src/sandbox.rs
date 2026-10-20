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
