//! Saving one of the TypeScript plugin's `[plugin.workspace] watch_files`
//! (`tsconfig*.json`, `jsconfig*.json`, `package.json`,
//! `pnpm-workspace.yaml`) reindexes TypeScript, through the real plugin
//! (docs/adr/0023-project-model-tracks-file-presence.md: the project model
//! reads its configs once, so only a reindex picks up an edit to one).
//!
//! Each case starts the plugin over a project whose config does not yet
//! resolve an import, then edits the config on disk so that it would. The
//! import resolves only if the language is walked again:
//! - routing the same file name under `node_modules` leaves it unresolved;
//! - routing the edited config itself resolves it.
//!
//! Controls: drop `under_excluded_dir` from
//! `PluginRegistry::workspace_language_matches` -> the `node_modules` route
//! reindexes and the import resolves too early; remove the file's pattern from
//! `watch_files` in `plugins/typescript/plugin.toml` -> the config route is
//! an ordinary file change and the import stays unresolved.

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

const TARGET: &str = "export function target(): number {\n  return 1;\n}\n";

struct Case {
    /// The watched config, project-relative.
    config: &'static str,
    /// Its contents at spawn (`None`: absent) and after the save.
    before: Option<&'static str>,
    after: &'static str,
    /// Other files, written before the plugin starts.
    files: &'static [(&'static str, &'static str)],
    importer: &'static str,
    specifier: &'static str,
    /// What the import resolves to after the reindex, as `file:name` of the
    /// target file node.
    resolved: &'static str,
}

fn importer_source(specifier: &str) -> String {
    format!("import {{ target }} from \"{specifier}\";\n\nexport const value = target();\n")
}

fn assert_a_saved_config_reindexes(case: Case) {
    let harness = Harness::new();
    for (path, contents) in case.files {
        harness.write(path, contents);
    }
    if let Some(before) = case.before {
        harness.write(case.config, before);
    }
    harness.write(case.importer, &importer_source(case.specifier));
    harness.route(case.importer);
    let (resolved, unresolved) = harness.imports(case.importer);
    assert_eq!(resolved, Vec::<String>::new(), "{}: nothing resolves before the save", case.config);
    assert_eq!(unresolved.len(), 1, "{}: the import is stored unresolved: {unresolved:?}", case.config);

    harness.write(case.config, case.after);
    let file_name = case.config.rsplit('/').next().unwrap();
    let ignored = format!("node_modules/dep/{file_name}");
    harness.write(&ignored, case.after);
    harness.route(&ignored);
    assert_eq!(
        harness.imports_of(case.importer, true),
        Vec::<String>::new(),
        "{ignored}: a watched name under node_modules reindexes nothing"
    );

    harness.route(case.config);
    assert_eq!(
        harness.imports(case.importer),
        (vec![case.resolved.to_string()], Vec::new()),
        "{}: saving it reindexes TypeScript and the import resolves",
        case.config
    );
}

#[test]
fn saving_tsconfig_json_reindexes_typescript() {
    assert_a_saved_config_reindexes(Case {
        config: "tsconfig.json",
        before: Some("{}\n"),
        after: r#"{"compilerOptions":{"paths":{"@/*":["./lib/*"]}}}"#,
        files: &[("lib/b.ts", TARGET)],
        importer: "app.ts",
        specifier: "@/b",
        resolved: "lib/b.ts:b.ts",
    });
}

/// `tsconfig*.json` is a glob: an `extends` target such as
/// `tsconfig.base.json` is watched as well.
#[test]
fn saving_an_extended_tsconfig_base_json_reindexes_typescript() {
    assert_a_saved_config_reindexes(Case {
        config: "tsconfig.base.json",
        before: Some("{}\n"),
        after: r#"{"compilerOptions":{"paths":{"@/*":["./lib/*"]}}}"#,
        files: &[("tsconfig.json", r#"{"extends":"./tsconfig.base.json"}"#), ("lib/b.ts", TARGET)],
        importer: "app.ts",
        specifier: "@/b",
        resolved: "lib/b.ts:b.ts",
    });
}

#[test]
fn saving_jsconfig_json_reindexes_typescript() {
    assert_a_saved_config_reindexes(Case {
        config: "jsconfig.json",
        before: Some("{}\n"),
        after: r#"{"compilerOptions":{"paths":{"@/*":["./lib/*"]}}}"#,
        files: &[("lib/b.js", "export function target() {\n  return 1;\n}\n")],
        importer: "app.js",
        specifier: "@/b",
        resolved: "lib/b.js:b.js",
    });
}

/// A `package.json` `imports` map.
#[test]
fn saving_package_json_reindexes_typescript() {
    assert_a_saved_config_reindexes(Case {
        config: "package.json",
        before: Some(r#"{"name":"app"}"#),
        after: r##"{"name":"app","imports":{"#b":"./lib/b.ts"}}"##,
        files: &[("lib/b.ts", TARGET)],
        importer: "app.ts",
        specifier: "#b",
        resolved: "lib/b.ts:b.ts",
    });
}

/// A `pnpm-workspace.yaml` created mid-session makes `packages/*` workspace
/// packages, so a bare package name resolves to one's entry.
#[test]
fn saving_pnpm_workspace_yaml_reindexes_typescript() {
    assert_a_saved_config_reindexes(Case {
        config: "pnpm-workspace.yaml",
        before: None,
        after: "packages:\n  - 'packages/*'\n",
        files: &[
            ("packages/math/package.json", r#"{"name":"@acme/math"}"#),
            ("packages/math/src/index.ts", TARGET),
        ],
        importer: "app.ts",
        specifier: "@acme/math",
        resolved: "packages/math/src/index.ts:index.ts",
    });
}
