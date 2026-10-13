//! The vtsls tier runs no code the indexed project supplies and starts no
//! `npm` (`docs/design/GM-566-typescript-no-project-code.md`, section 7,
//! fixture `fx`; ADR 0032 and 0033).
//!
//! Each test runs the plugin's own [`semantic::engine`] with the shipped
//! manifest and its bare `vtsls` command, so resolution and the settings
//! sent to vtsls are the production ones:
//!
//! - a logging `node_modules/.bin/vtsls` in the project, with no vtsls on
//!   `PATH` and a logging `npx` on `PATH`: neither may be probed or run, so
//!   no server resolves at all;
//! - a project `node_modules/@vtsls/language-server` with a *real* `npx` and
//!   `node` on `PATH` (no vtsls): `npx` there would run the project's
//!   package, so it must never run, and the tier is off with the remedy;
//! - `fx` with vtsls on `PATH`: the project's `typescript` and its tsconfig
//!   `plugins` package never load, and automatic type acquisition runs no
//!   `npm`, while the project's tsconfig is still the one in force.
//!
//! The shims are shell scripts, so the whole file is unix only. The tests
//! rewrite `PATH`, `HOME` and the working directory of this process, so they
//! hold one lock.

#![cfg(unix)]

mod no_project_code;

use std::path::{Path, PathBuf};
use std::process::Command;

use g_mesh_plugin_typescript::semantic;
use no_project_code::{
    absolute, assert_npm_reaches, assert_the_tsconfig_was_in_force, index, installed_bin, markers, pass,
    route_npm_to, run, set_environment, shim, write, write_fx, written, Scratch, Sink, ATA_WINDOW, A_TS,
    ENVIRONMENT,
};

/// Control C1: in `plugins/typescript/src/semantic.rs`, `candidates`, add the
/// project's `node_modules/.bin/<command>` spellings after `PATH`'s.
#[test]
fn the_projects_own_vtsls_is_never_probed_or_run() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let installed = installed_bin();
    let node = absolute("node");
    let scratch = Scratch::new("local");
    let root = scratch.0.join("project");
    let logs_dir = scratch.0.join("logs");
    let local_log = logs_dir.join("project-vtsls");
    let npx_log = logs_dir.join("npx");

    write(&root.join("src/a.ts"), A_TS);
    let local_bin = root.join("node_modules/.bin");
    let real = installed.join("vtsls").canonicalize().unwrap();
    shim(
        &local_bin.join("vtsls"),
        &local_log,
        &format!("exec '{}' '{}' \"$@\"", node.display(), real.display()),
    );
    let shims = scratch.0.join("shims");
    shim(&shims.join("npx"), &npx_log, "exit 1");

    set_environment(&[shims, PathBuf::from("/usr/bin"), PathBuf::from("/bin")], &root, &scratch.0);

    // Sanity: the project's copy is a working vtsls and logs, and no other
    // vtsls is reachable.
    run(&local_bin.join("vtsls"), &["--version"], &root);
    assert_eq!(written(std::slice::from_ref(&local_log)).len(), 1, "sanity: the project's vtsls logs");
    let _ = std::fs::remove_file(&local_log);
    assert!(Command::new("vtsls").arg("--version").output().is_err(), "sanity: no vtsls on PATH");

    let engine = semantic::engine(&root);

    assert_eq!(
        written(std::slice::from_ref(&local_log)),
        Vec::<String>::new(),
        "the plugin probed or ran the project's node_modules/.bin/vtsls"
    );
    assert!(
        !npx_log.exists(),
        "npx was run: in the project it runs the project's own node_modules/@vtsls/language-server"
    );
    let message = format!("{:#}", engine.err().expect("with no vtsls on PATH, nothing may resolve"));
    assert!(
        message.contains("npm install -g @vtsls/language-server") && message.contains("PATH"),
        "remedy missing: {message}"
    );
}

/// The test above stubs `npx` with `exit 1`, so it cannot see what a real
/// `npx` does in the project. Here `npx` is the real one (behind a logging
/// shim), with `node` beside it and no vtsls on `PATH`, and the project ships
/// `node_modules/@vtsls/language-server` whose bin writes a marker.
/// `npx --yes --package @vtsls/language-server vtsls …` run with the project
/// as cwd finds that local package and runs it - checked as a sanity step -
/// so the plugin must not reach `npx` at all, and must report the tier off
/// with the remedy.
///
/// Control C2: in `plugins/typescript/src/semantic.rs`, `candidates`, append
/// an `npx --yes --package @vtsls/language-server vtsls` candidate.
#[test]
fn a_real_npx_in_the_project_never_runs_the_projects_vtsls() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let node = absolute("node");
    let npx = absolute("npx");
    let scratch = Scratch::new("npx");
    let root = scratch.0.join("project");
    let logs_dir = scratch.0.join("logs");
    let marker = logs_dir.join("project-vtsls-package");
    let npx_log = logs_dir.join("npx");

    write(&root.join("src/a.ts"), A_TS);
    let package = root.join("node_modules/@vtsls/language-server");
    write(
        &package.join("package.json"),
        "{\"name\": \"@vtsls/language-server\", \"version\": \"0.3.999\", \"bin\": {\"vtsls\": \"bin/vtsls.js\"}}\n",
    );
    std::fs::create_dir_all(&logs_dir).unwrap();
    let script = package.join("bin/vtsls.js");
    write(
        &script,
        &format!(
            "#!/usr/bin/env node\nrequire('fs').appendFileSync({:?}, process.argv.slice(1).join(' ') + \
             '\\n');\nconsole.log('0.3.999');\n",
            marker.display().to_string()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let bin_dir = root.join("node_modules/.bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::os::unix::fs::symlink("../@vtsls/language-server/bin/vtsls.js", bin_dir.join("vtsls")).unwrap();
    let shims = scratch.0.join("shims");
    std::fs::create_dir_all(&shims).unwrap();
    std::os::unix::fs::symlink(&node, shims.join("node")).unwrap();
    shim(&shims.join("npx"), &npx_log, &format!("exec '{}' \"$@\"", npx.display()));

    set_environment(&[shims, PathBuf::from("/usr/bin"), PathBuf::from("/bin")], &root, &scratch.0);

    // Sanity: no vtsls on PATH, and the real npx in the project runs the
    // project's package (the probe an npx candidate would run).
    assert!(Command::new("vtsls").arg("--version").output().is_err(), "sanity: no vtsls on PATH");
    run(Path::new("npx"), &["--yes", "--package", "@vtsls/language-server", "vtsls", "--version"], &root);
    assert!(
        marker.exists(),
        "sanity: npx in the project runs the project's node_modules/@vtsls/language-server"
    );
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&npx_log);

    let engine = semantic::engine(&root);

    assert_eq!(
        written(&[marker, npx_log]),
        Vec::<String>::new(),
        "the plugin ran npx, which ran the project's own node_modules/@vtsls/language-server"
    );
    let message = format!("{:#}", engine.err().expect("with no vtsls on PATH, the semantic tier is off"));
    assert!(
        message.contains("npm install -g @vtsls/language-server") && message.contains("PATH"),
        "remedy missing: {message}"
    );
}

/// `fx` with vtsls on `PATH` and every npm network variable pointing at a
/// counting sink. A whole pass resolves `made.method()` through the tsconfig's
/// `paths` alias - so the hostile tsconfig is the one in force - and yet:
///
/// - no `MARK_*` file appears: neither the project's `node_modules/typescript`
///   nor the tsconfig `plugins` package `evil-ts-plugin` was loaded;
/// - the sink sees no connection while the server stays up for
///   [`ATA_WINDOW`]: automatic type acquisition on `loose.js` ran no `npm`.
///
/// Control C3: `vtsls.autoUseWorkspaceTsdk = true` in `plugin.toml` (the
/// markers). Control C4: drop `typescript.disableAutomaticTypeAcquisition`
/// from `plugin.toml` (the sink).
#[test]
fn the_shipped_settings_load_no_project_typescript_or_plugin_and_run_no_npm() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let installed = installed_bin();
    let node = absolute("node");
    let node_dir = node.parent().unwrap().to_path_buf();
    let scratch = Scratch::new("settings");
    let root = scratch.0.join("project");
    let sink = Sink::start();
    write_fx(&root);

    set_environment(
        &[installed, node_dir, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        &root,
        &scratch.0,
    );
    route_npm_to(&sink);
    let elsewhere = scratch.0.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    assert_npm_reaches(&sink, &node, &elsewhere);
    assert_eq!(markers(&root), Vec::<String>::new(), "sanity: writing the fixture ran none of it");

    let index = index(&root);
    let (engine, answer) = pass(&root, &index);
    assert_the_tsconfig_was_in_force(&index, &answer);
    let attempts = sink.take_within(ATA_WINDOW);
    drop(engine);

    assert_eq!(
        markers(&root),
        Vec::<String>::new(),
        "the project's own typescript or its tsconfig plugin package ran in the server"
    );
    assert_eq!(attempts, Vec::<String>::new(), "automatic type acquisition ran npm against the registry");
}
