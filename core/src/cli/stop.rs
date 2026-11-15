//! `g-mesh stop`: the command around `control::stop`, which stops the current
//! project's daemon core and every language plugin it has spawned.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use crate::control::stop::{self, Outcome, Stopped};

/// Stops the daemon serving the current directory's project.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    let outcome = stop::stop(&cwd)?;
    print!("{}", render(&outcome, &cwd));
    Ok(())
}

/// Renders an outcome as the text the command prints.
pub fn render(outcome: &Outcome, project_root: &Path) -> String {
    if !outcome.stopped_anything() {
        return format!("g-mesh: no daemon is running for {}\n", project_root.display());
    }

    let mut out = format!("g-mesh: stopped the daemon for {}\n", project_root.display());
    if let Some(core) = outcome.core {
        let _ = writeln!(out, "  daemon core: pid {} ({})", core.pid(), describe(core));
        if outcome.core_was_wedged {
            let _ = writeln!(
                out,
                "    it had removed its own pid file but was still holding this project's \
                 daemon lock, which is why nothing could connect to it"
            );
        }
    }
    for (language, plugin) in &outcome.plugins {
        let _ = writeln!(out, "  plugin ({language}): pid {} ({})", plugin.pid(), describe(*plugin));
    }
    out
}

fn describe(stopped: Stopped) -> &'static str {
    match stopped {
        Stopped::Terminated { .. } => "terminated",
        Stopped::Killed { .. } => "killed after ignoring SIGTERM",
        Stopped::ExitedWithCore { .. } => "exited with its core",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn nothing_running_renders_as_a_no_op_rather_than_a_shutdown() {
        let outcome = Outcome::default();
        assert!(!outcome.stopped_anything());

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));
        assert_eq!(rendered, "g-mesh: no daemon is running for /tmp/project\n");
    }

    #[test]
    fn a_shutdown_renders_both_processes_and_how_each_one_went() {
        let outcome = Outcome {
            core: Some(Stopped::Terminated { pid: 111 }),
            plugins: vec![("typescript".to_string(), Stopped::ExitedWithCore { pid: 222 })],
            ..Outcome::default()
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("stopped the daemon for /tmp/project"), "{rendered}");
        assert!(rendered.contains("daemon core: pid 111 (terminated)"), "{rendered}");
        assert!(rendered.contains("plugin (typescript): pid 222 (exited with its core)"), "{rendered}");
    }

    /// Two languages each get their own line, independently described.
    #[test]
    fn a_shutdown_with_two_languages_renders_one_line_each() {
        let outcome = Outcome {
            core: Some(Stopped::Terminated { pid: 111 }),
            plugins: vec![
                ("go".to_string(), Stopped::ExitedWithCore { pid: 222 }),
                ("typescript".to_string(), Stopped::Killed { pid: 333 }),
            ],
            ..Outcome::default()
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("plugin (go): pid 222 (exited with its core)"), "{rendered}");
        assert!(
            rendered.contains("plugin (typescript): pid 333 (killed after ignoring SIGTERM)"),
            "{rendered}"
        );
    }

    #[test]
    fn an_escalated_shutdown_says_so() {
        let outcome = Outcome {
            core: Some(Stopped::Killed { pid: 111 }),
            plugins: vec![("typescript".to_string(), Stopped::Killed { pid: 222 })],
            ..Outcome::default()
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("pid 111 (killed after ignoring SIGTERM)"), "{rendered}");
        assert!(rendered.contains("pid 222 (killed after ignoring SIGTERM)"), "{rendered}");
    }
}
