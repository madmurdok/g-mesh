use super::*;

fn spellings(list: &[&str]) -> Vec<String> {
    list.iter().map(|path| path.to_string()).collect()
}

/// The folding decision on both kinds of volume, whichever this machine's
/// is: folded keys lower the case, exact keys keep it.
///
/// Control: make `fold_key` ignore `fold` (always lower, or never lower) -
/// one of the two arms fails.
#[test]
fn a_key_is_lowercased_only_when_the_volume_folds_case() {
    assert_eq!(fold_key(true, "Cfg/Base.JSON"), "cfg/base.json");
    assert_eq!(fold_key(false, "Cfg/Base.JSON"), "Cfg/Base.JSON");
    assert_eq!(fold_key(true, "cfg/base.json"), "cfg/base.json");
}

/// Two spellings of one file are one key where case folds and two where it
/// does not; the keys come back sorted either way.
#[test]
fn keys_are_sorted_and_two_spellings_merge_only_where_case_folds() {
    let paths = spellings(&["b.json", "Cfg/Base.json", "cfg/base.json"]);
    assert_eq!(fold_keys(true, &paths), spellings(&["b.json", "cfg/base.json"]));
    assert_eq!(fold_keys(false, &paths), spellings(&["Cfg/Base.json", "b.json", "cfg/base.json"]));
}

/// The probe agrees with an independent look at the volume: a file created
/// as `Probe.TXT` is reachable as `probe.txt` exactly when `folds_case` says
/// so, and `key` and `keys` follow it.
#[test]
fn the_probe_agrees_with_what_the_volume_does() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    fs::write(root.join("Probe.TXT"), "").unwrap();
    let measured = fs::metadata(root.join("probe.txt")).is_ok();

    assert_eq!(folds_case(&root), measured);
    assert_eq!(key(&root, "A/B.json"), if measured { "a/b.json" } else { "A/B.json" });
    assert_eq!(
        keys(&root, &spellings(&["A.json", "a.json"])),
        if measured { spellings(&["a.json"]) } else { spellings(&["A.json", "a.json"]) }
    );
}

/// A root whose entries have no ASCII letter is probed through its own last
/// component, and still agrees with the volume.
#[test]
fn a_root_without_lettered_entries_is_probed_through_its_own_name() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().canonicalize().unwrap().join("Project-Root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("123"), "").unwrap();
    fs::write(root.join("-_"), "").unwrap();
    let measured = fs::metadata(root.with_file_name("project-root")).is_ok();

    assert_eq!(folds_case(&root), measured);
}

/// A name with no ASCII letter has no other spelling; one with letters has
/// every letter's case swapped and non-ASCII characters kept.
#[test]
fn swapping_case_needs_an_ascii_letter() {
    assert_eq!(swap_ascii_case("123-_.1"), None);
    assert_eq!(swap_ascii_case("aB1é"), Some("Ab1é".to_string()));
}
