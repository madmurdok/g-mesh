//! What the install scripts tell a person after installing: an upgrade from a
//! release without the rerank model needs `g-mesh model fetch` again.

const INSTALL_SH: &str = include_str!("../../../scripts/install.sh");
const INSTALL_PS1: &str = include_str!("../../../scripts/install.ps1");

/// *Control:* drop the "Upgrading from a release before 3.18?" lines from
/// either script -> that script's arm fails.
#[test]
fn both_install_scripts_say_to_fetch_again_after_an_upgrade() {
    for (name, script) in [("install.sh", INSTALL_SH), ("install.ps1", INSTALL_PS1)] {
        assert!(
            script.contains("Upgrading from a release before 3.18? Run g-mesh model fetch again"),
            "{name} must say to re-run `g-mesh model fetch` after an upgrade"
        );
        assert!(script.contains("the rerank model"), "{name} must name what the fetch adds");
    }
}
