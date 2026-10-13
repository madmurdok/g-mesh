//! A full index plus semantic pass over a TypeScript project reaches no
//! network and writes nothing under the project root
//! (`docs/design/GM-566-typescript-no-project-code.md` section 7, ADR 0033).
//!
//! The fixture is `fx` (`no_project_code::write_fx`): a tsconfig whose
//! `paths` alias `src/a.ts` imports through and whose `plugins` name a
//! project package, a project `typescript` that writes a marker when loaded,
//! and `loose.js` requiring a `package.json` dependency - the automatic type
//! acquisition trigger. Every npm network variable points at a local sink
//! that logs every connection and answers 403; the sanity step checks the
//! `npm` beside `node`, the one tsserver's typings installer runs, reaches
//! it.
//!
//! The pass runs through the plugin's own [`semantic::engine`] with the
//! shipped manifest. Then `made.method()` must resolve through the alias
//! (the server really answered, with this tsconfig), and, after the server
//! has stayed up for [`ATA_WINDOW`], the root's snapshot - every path with
//! its type, size and content hash - must be unchanged and the sink must
//! have seen no connection.
//!
//! `semantic_runs_no_project_code.rs` holds the controls (C3 and C4 fail this
//! test too). Unix only, like that file. One test in this binary, because it
//! rewrites the process environment.

#![cfg(unix)]

mod no_project_code;

use std::path::PathBuf;

use no_project_code::{
    absolute, assert_npm_reaches, assert_the_tsconfig_was_in_force, changes, index, installed_bin, pass,
    route_npm_to, set_environment, snapshot, write_fx, Scratch, Sink, ATA_WINDOW,
};

#[test]
fn indexing_reaches_no_network_and_writes_nothing_under_the_root() {
    let installed = installed_bin();
    let node = absolute("node");
    let node_dir = node.parent().unwrap().to_path_buf();
    let scratch = Scratch::new("offline");
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

    let before = snapshot(&root);
    let index = index(&root);
    let (engine, answer) = pass(&root, &index);
    assert_the_tsconfig_was_in_force(&index, &answer);
    let attempts = sink.take_within(ATA_WINDOW);
    drop(engine);

    assert_eq!(attempts, Vec::<String>::new(), "the semantic pass made network connections");
    assert_eq!(
        changes(&before, &snapshot(&root)),
        Vec::<String>::new(),
        "the semantic pass wrote under the root"
    );
}
