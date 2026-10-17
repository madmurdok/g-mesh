//! `scripts/ci-junit-summary.py` (GM-344) turns the CI JUnit files into the
//! run page's failure table. Its rows are test binaries, except for core's two
//! merged integration-test binaries (GM-623), whose rows are split back into
//! one per module, the former test file, so a failure stays attributable to
//! the file it came from. Run for real against hand-written JUnit; python3
//! only, so Unix only, like the other script tests.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// One `<testcase>`: (name, failed).
type Case<'a> = (&'a str, bool);

/// A nextest-shaped JUnit document holding one `<testsuite>` per binary.
fn junit(suites: &[(&str, &[Case])]) -> String {
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites name=\"nextest-run\">\n");
    for (binary, cases) in suites {
        let failures = cases.iter().filter(|(_, failed)| *failed).count();
        xml.push_str(&format!(
            "  <testsuite name=\"{binary}\" tests=\"{}\" disabled=\"0\" errors=\"0\" failures=\"{failures}\">\n",
            cases.len()
        ));
        for (name, failed) in *cases {
            if *failed {
                xml.push_str(&format!(
                    "    <testcase name=\"{name}\" classname=\"{binary}\" time=\"0.1\">\n      \
                     <failure type=\"test failure\">boom</failure>\n    </testcase>\n"
                ));
            } else {
                xml.push_str(&format!(
                    "    <testcase name=\"{name}\" classname=\"{binary}\" time=\"0.1\"/>\n"
                ));
            }
        }
        xml.push_str("  </testsuite>\n");
    }
    xml.push_str("</testsuites>\n");
    xml
}

fn summarise(documents: &[String]) -> String {
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    let mut args: Vec<PathBuf> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let path = dir.path().join(format!("junit-{index}.xml"));
        fs::write(&path, document).expect("failed to write the JUnit file");
        args.push(path);
    }
    let output = Command::new("python3")
        .arg(repo_root().join("scripts/ci-junit-summary.py"))
        .args(&args)
        .arg("x86_64-unknown-linux-gnu")
        .output()
        .expect("failed to run python3");
    assert!(
        output.status.success(),
        "ci-junit-summary.py failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("the summary is not UTF-8")
}

fn table_rows(summary: &str) -> Vec<&str> {
    summary
        .lines()
        .filter(|line| line.starts_with("| ") && !line.starts_with("| failed") && !line.starts_with("| ---"))
        .collect()
}

#[test]
fn failures_in_two_modules_of_the_merged_it_binary_are_two_rows() {
    let summary = summarise(&[junit(&[(
        "g-mesh::it",
        &[
            ("cli_status::a", true),
            ("cli_status::b", true),
            ("cli_status::c", false),
            ("daemon_core::a", true),
            ("daemon_core::b", false),
            ("mcp_e2e::a", false),
        ],
    )])]);
    assert!(summary.contains("**3** of **6** tests failed, in **2** binaries."), "{summary}");
    assert_eq!(
        table_rows(&summary),
        ["| 2 | 3 | `g-mesh::it::cli_status` |", "| 1 | 2 | `g-mesh::it::daemon_core` |"],
        "{summary}"
    );
}

#[test]
fn the_scripts_binary_is_split_and_every_other_binary_stays_one_row() {
    let summary = summarise(&[
        junit(&[
            ("g-mesh", &[("mcp::tests::a", true), ("graph::tests::b", true)]),
            ("g-mesh::scripts", &[("test_local_script::a", true), ("test_select_scripts::a", false)]),
        ]),
        // A binary split across two section files is still summed into one row.
        junit(&[("g-mesh", &[("daemon::tests::c", true)])]),
    ]);
    assert_eq!(
        table_rows(&summary),
        ["| 3 | 3 | `g-mesh` |", "| 1 | 1 | `g-mesh::scripts::test_local_script` |"],
        "{summary}"
    );
}

#[test]
fn a_passing_run_counts_every_test_of_the_merged_binaries() {
    let summary = summarise(&[junit(&[
        ("g-mesh::it", &[("a::x", false), ("b::y", false)]),
        ("g-mesh::scripts", &[("c::z", false)]),
        ("g-mesh", &[("lib::t", false)]),
    ])]);
    assert!(summary.contains("All **4** tests passed."), "{summary}");
}
