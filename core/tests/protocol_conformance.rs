use g_mesh::protocol::conformance::{check_bulk_output, check_control_plane_output};

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()))
}

#[test]
fn well_formed_ndjson_fixture_is_conformant() {
    let report = check_bulk_output(&fixture("valid.ndjson"));
    assert!(report.is_conformant(), "{:?}", report.violations);
}

#[test]
fn ndjson_fixture_with_invalid_edge_kind_is_rejected() {
    let report = check_bulk_output(&fixture("invalid_kind.ndjson"));
    assert!(!report.is_conformant());
    assert!(
        report.violations.iter().any(|v| v.message.contains("neither a valid node nor edge")),
        "{:?}",
        report.violations
    );
}

#[test]
fn well_formed_control_plane_fixture_is_conformant() {
    let report = check_control_plane_output(&fixture("valid_control.rpc"));
    assert!(report.is_conformant(), "{:?}", report.violations);
}

#[test]
fn broken_framing_fixture_is_rejected_with_a_specific_error() {
    let report = check_control_plane_output(&fixture("broken_framing.rpc"));
    assert!(!report.is_conformant());
    assert!(
        report.violations.iter().any(|v| v.message.to_lowercase().contains("content-length")),
        "{:?}",
        report.violations
    );
}
