//! `overrides`: the probe against a hand-built index, per mode. The handler
//! cases (capability gating, both wire paths, the once-per-session hint) sit
//! in `find_callers_callees`' own tests. Design:
//! `docs/architecture/gm-502-override-callers-field.md`, section 5.

use rusqlite::Connection;

use super::*;
use crate::graph::queries;
use crate::graph::symbol_links::PENDING_SYMBOL_NATIVE_KIND;
use crate::protocol::types::{PathSegment, QualifiedPath};
use crate::storage::schema;
use crate::storage::write::{apply_diff, Diff, EdgeRecord};

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn
}

/// Where a node sits: its language, the separator its path joins segments
/// with, its file and its container (`None`: TypeScript's file scope).
#[derive(Clone, Copy)]
struct At<'a> {
    language: &'a str,
    sep: &'a str,
    file: &'a str,
    container: Option<&'a str>,
}

const PY_SUB: At = At { language: "python", sep: ".", file: "pkg/sub.py", container: Some("pkg.sub") };
const PY_BASE: At = At { language: "python", sep: ".", file: "pkg/base.py", container: Some("pkg.base") };
const RUST: At = At { language: "rust", sep: "::", file: "src/shapes.rs", container: Some("krate::shapes") };

/// A `kind` declaration `id` at `segments`, read back as the index stores it.
fn node(conn: &mut Connection, at: At, id: &str, kind: &str, segments: &[&str]) -> NodeRecord {
    let qualified_name = segments.join(at.sep);
    let mut node =
        NodeRecord::new(id, kind, *segments.last().unwrap(), &qualified_name, at.file, at.language);
    node.container = at.container.map(str::to_string);
    node.qualified_path = Some(QualifiedPath(
        segments
            .iter()
            .enumerate()
            .map(|(i, name)| PathSegment { sep: (i > 0).then(|| at.sep.to_string()), name: name.to_string() })
            .collect(),
    ));
    apply_diff(conn, &Diff { upsert_nodes: vec![node], ..Default::default() }).unwrap();
    queries::get_node(conn, id).unwrap().unwrap()
}

fn ty(conn: &mut Connection, at: At, id: &str, segments: &[&str]) -> NodeRecord {
    node(conn, at, id, "Type", segments)
}

fn function(conn: &mut Connection, at: At, id: &str, segments: &[&str]) -> NodeRecord {
    node(conn, at, id, "Function", segments)
}

/// `from -SUPERTYPE_OF-> to`, `resolved` or not.
fn supertype(conn: &mut Connection, from: &str, to: &str, resolved: bool) {
    let edge = EdgeRecord::new(format!("{from}->{to}"), from, to, "SUPERTYPE_OF", "syntactic", resolved);
    apply_diff(conn, &Diff { upsert_edges: vec![edge], ..Default::default() }).unwrap();
}

/// The reported members' qualified names, `[]` for an absent field.
fn names(found: Option<Overrides>) -> Vec<String> {
    found.map_or_else(Vec::new, |found| found.members.into_iter().map(|row| row.qualified_name).collect())
}

/// One Python `class C(B)` with `B` a project class, per `chain` entry
/// `(class, its base or None, declares m)`, all in `pkg/sub.py`; the anchor
/// is the last class's `m`.
fn python_chain(conn: &mut Connection, chain: &[(&str, Option<&str>, bool)]) -> NodeRecord {
    let mut anchor = None;
    for (class, base, declares) in chain {
        ty(conn, PY_SUB, class, &["pkg", "sub", class]);
        if *declares {
            let id = format!("{class}.m");
            anchor = Some(function(conn, PY_SUB, &id, &["pkg", "sub", class, "m"]));
        }
        if let Some(base) = base {
            supertype(conn, class, base, true);
        }
    }
    anchor.expect("the last class declares m")
}

// --- B1: by_name, a base in another file ------------------------------------

/// `Sub.describe` overriding `Base.describe` one file over names it, with the
/// id `find_callers(symbol_id=...)` takes, its file and its zero-based line;
/// `BaseExtra.describe` (in the range scan, other owner) and
/// `Base.Inner.describe` (one segment too deep) are not members of `Base`.
/// Control: `probe` returns `None` unconditionally.
#[test]
fn by_name_names_a_base_member_in_another_file() {
    let mut conn = setup();
    ty(&mut conn, PY_BASE, "Base", &["pkg", "base", "Base"]);
    let mut base_member = function(&mut conn, PY_BASE, "Base.describe", &["pkg", "base", "Base", "describe"]);
    base_member.start_line = 11;
    apply_diff(&mut conn, &Diff { upsert_nodes: vec![base_member], ..Default::default() }).unwrap();
    ty(&mut conn, PY_BASE, "BaseExtra", &["pkg", "base", "BaseExtra"]);
    function(&mut conn, PY_BASE, "BaseExtra.describe", &["pkg", "base", "BaseExtra", "describe"]);
    ty(&mut conn, PY_BASE, "Base.Inner", &["pkg", "base", "Base", "Inner"]);
    function(&mut conn, PY_BASE, "Base.Inner.describe", &["pkg", "base", "Base", "Inner", "describe"]);

    ty(&mut conn, PY_SUB, "Sub", &["pkg", "sub", "Sub"]);
    let anchor = function(&mut conn, PY_SUB, "Sub.describe", &["pkg", "sub", "Sub", "describe"]);
    supertype(&mut conn, "Sub", "Base", true);

    let found = probe(&conn, &anchor, MemberOverrides::ByName).expect("Sub.describe overrides Base.describe");
    assert!(!found.truncated);
    assert_eq!(
        serde_json::to_value(&found.members).unwrap(),
        serde_json::json!([{
            "id": "Base.describe",
            "qualifiedName": "pkg.base.Base.describe",
            "filePath": "pkg/base.py",
            "startLine": 11,
        }])
    );
    assert_eq!(found.file_paths().collect::<Vec<_>>(), vec!["pkg/base.py"]);
}

// --- B2: absent -------------------------------------------------------------

/// No field for: a free function, a method of a class with no supertype, a
/// method no supertype declares (the base declares another name), a base
/// that is an unresolved placeholder, and a non-`Function` anchor.
/// Control: drop `name = ?1` from `member_of` (the base's `other` is
/// reported for `Sub.describe`).
#[test]
fn nothing_to_name_is_no_field() {
    let mut conn = setup();
    let free = function(&mut conn, PY_SUB, "helper", &["pkg", "sub", "helper"]);
    // `pkg.sub` is a module, not a `Type`: no owner.
    let free_in_module = function(&mut conn, PY_SUB, "sub.helper", &["sub", "helper"]);

    ty(&mut conn, PY_SUB, "Lone", &["pkg", "sub", "Lone"]);
    let lone = function(&mut conn, PY_SUB, "Lone.describe", &["pkg", "sub", "Lone", "describe"]);

    ty(&mut conn, PY_BASE, "Base", &["pkg", "base", "Base"]);
    function(&mut conn, PY_BASE, "Base.other", &["pkg", "base", "Base", "other"]);
    ty(&mut conn, PY_SUB, "Sub", &["pkg", "sub", "Sub"]);
    let undeclared = function(&mut conn, PY_SUB, "Sub.describe", &["pkg", "sub", "Sub", "describe"]);
    supertype(&mut conn, "Sub", "Base", true);

    let mut external = NodeRecord::new("ext", "Type", "TestCase", "unittest.TestCase", "pkg/t.py", "python");
    external.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
    apply_diff(&mut conn, &Diff { upsert_nodes: vec![external], ..Default::default() }).unwrap();
    ty(&mut conn, PY_SUB, "Case", &["pkg", "sub", "Case"]);
    let on_placeholder = function(&mut conn, PY_SUB, "Case.describe", &["pkg", "sub", "Case", "describe"]);
    supertype(&mut conn, "Case", "ext", false);

    let not_a_function = node(&mut conn, PY_SUB, "Sub.attr", "Variable", &["pkg", "sub", "Sub", "other"]);

    for (what, anchor) in [
        ("free function", &free),
        ("free function, module path", &free_in_module),
        ("no supertype", &lone),
        ("no supertype declares the name", &undeclared),
        ("placeholder base", &on_placeholder),
        ("not a function", &not_a_function),
    ] {
        for mode in [MemberOverrides::ByName, MemberOverrides::Declared, MemberOverrides::None] {
            assert_eq!(probe(&conn, anchor, mode), None, "{what} under {mode}");
        }
    }
}

// --- B3: the nearest declaring ancestor -----------------------------------------

/// `A.m`, `B(A)` without `m`, `C(B).m`: the walk climbs through `B` and names
/// `A.m`. Control: `MAX_DEPTH = 1` (nothing past `B`).
#[test]
fn the_walk_climbs_through_a_type_that_does_not_declare_the_member() {
    let mut conn = setup();
    let anchor =
        python_chain(&mut conn, &[("A", None, true), ("B", Some("A"), false), ("C", Some("B"), true)]);
    assert_eq!(names(probe(&conn, &anchor, MemberOverrides::ByName)), vec!["pkg.sub.A.m"]);
}

/// With `B.m` added, only `B.m`: the walk stops at the nearest declaring
/// ancestor (D2's stop rule) and `B.m`'s own page names `A.m`.
/// Control: keep climbing after a declaring type (enqueue it in the `Some`
/// arm of `walk_up`) -> `[B.m, A.m]`.
#[test]
fn the_walk_stops_at_the_nearest_declaring_ancestor() {
    let mut conn = setup();
    let anchor =
        python_chain(&mut conn, &[("A", None, true), ("B", Some("A"), true), ("C", Some("B"), true)]);
    assert_eq!(names(probe(&conn, &anchor, MemberOverrides::ByName)), vec!["pkg.sub.B.m"]);
    let b = queries::get_node(&conn, "B.m").unwrap().unwrap();
    assert_eq!(
        names(probe(&conn, &b, MemberOverrides::ByName)),
        vec!["pkg.sub.A.m"],
        "the chain is followable"
    );
}

/// Two branches: `C(Z, Y)`, `Z.m`, `Y(X)` without `m`, `X.m`. Each branch
/// reports its own nearest declaration, ordered by depth first, then
/// `qualifiedName` (`Z.m` at depth 1 before `X.m` at depth 2).
#[test]
fn each_branch_reports_its_nearest_declaration_ordered_by_depth() {
    let mut conn = setup();
    let anchor = python_chain(
        &mut conn,
        &[("X", None, true), ("Y", Some("X"), false), ("Z", None, true), ("C", None, true)],
    );
    supertype(&mut conn, "C", "Z", true);
    supertype(&mut conn, "C", "Y", true);
    assert_eq!(names(probe(&conn, &anchor, MemberOverrides::ByName)), vec!["pkg.sub.Z.m", "pkg.sub.X.m"]);
}

/// A supertype cycle in bad data ends the walk instead of looping.
#[test]
fn a_supertype_cycle_ends_the_walk() {
    let mut conn = setup();
    let anchor = python_chain(&mut conn, &[("A", None, false), ("B", Some("A"), true)]);
    supertype(&mut conn, "A", "B", true);
    assert_eq!(probe(&conn, &anchor, MemberOverrides::ByName), None);
}

// --- B4: owner scoping ---------------------------------------------------------

/// Go has a `T` in every package: two `T`s with the same `qualifiedName`,
/// each satisfying its own package's interface. `pb`'s `T.M` names only
/// `I2.M`. Control: drop the scope condition from `owner_of` (`ORDER BY id`
/// picks `pa`'s `T`, whose base is `I1`).
#[test]
fn the_owner_is_the_type_in_the_anchors_own_package() {
    let mut conn = setup();
    let pa = At { language: "go", sep: ".", file: "pa/t.go", container: Some("pa") };
    let pb = At { language: "go", sep: ".", file: "pb/t.go", container: Some("pb") };
    ty(&mut conn, pa, "pa:T", &["T"]);
    ty(&mut conn, pa, "pa:I1", &["I1"]);
    function(&mut conn, pa, "pa:I1.M", &["I1", "M"]);
    supertype(&mut conn, "pa:T", "pa:I1", true);
    ty(&mut conn, pb, "pb:T", &["T"]);
    ty(&mut conn, pb, "pb:I2", &["I2"]);
    function(&mut conn, pb, "pb:I2.M", &["I2", "M"]);
    supertype(&mut conn, "pb:T", "pb:I2", true);
    let anchor = function(&mut conn, pb, "pb:T.M", &["T", "M"]);

    let found = probe(&conn, &anchor, MemberOverrides::ByName).unwrap();
    assert_eq!(found.members.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), vec!["pb:I2.M"]);
}

/// TypeScript has no container: the file scopes the owner. `C` in `w.ts`
/// extends `B`; `C` in `x.ts` extends nothing, so its `m` overrides nothing.
/// Control: drop the scope condition from `owner_of` (`w.ts`'s `C` is
/// picked and `B.m` reported).
#[test]
fn without_a_container_the_owner_is_the_type_in_the_anchors_file() {
    let mut conn = setup();
    let w = At { language: "typescript", sep: ".", file: "w.ts", container: None };
    let x = At { language: "typescript", sep: ".", file: "x.ts", container: None };
    ty(&mut conn, w, "w.ts:B", &["B"]);
    function(&mut conn, w, "w.ts:B.m", &["B", "m"]);
    ty(&mut conn, w, "w.ts:C", &["C"]);
    let in_w = function(&mut conn, w, "w.ts:C.m", &["C", "m"]);
    supertype(&mut conn, "w.ts:C", "w.ts:B", true);
    ty(&mut conn, x, "x.ts:C", &["C"]);
    let in_x = function(&mut conn, x, "x.ts:C.m", &["C", "m"]);

    assert_eq!(probe(&conn, &in_x, MemberOverrides::ByName), None);
    assert_eq!(names(probe(&conn, &in_w, MemberOverrides::ByName)), vec!["B.m"]);
}

// --- B5 (probe half): the mode decides --------------------------------------------

/// A Rust inherent `Square::area` beside `Square -SUPERTYPE_OF-> Shape` and
/// `Shape::area` implements nothing: `declared` (Rust's mode) names nothing,
/// while a by-name rule would name `Shape::area`, which is why Rust is not
/// `by_name` (D1). `none` names nothing. The handler half (the mode read
/// from the capability map) is in `find_callers_callees`' tests.
#[test]
fn the_mode_decides_for_an_inherent_rust_method() {
    let mut conn = setup();
    ty(&mut conn, RUST, "Square", &["shapes", "Square"]);
    ty(&mut conn, RUST, "Shape", &["shapes", "Shape"]);
    function(&mut conn, RUST, "Shape::area", &["shapes", "Shape", "area"]);
    supertype(&mut conn, "Square", "Shape", true);
    let inherent = function(&mut conn, RUST, "Square::area", &["shapes", "Square", "area"]);

    assert_eq!(probe(&conn, &inherent, MemberOverrides::Declared), None);
    assert_eq!(probe(&conn, &inherent, MemberOverrides::None), None);
    assert_eq!(names(probe(&conn, &inherent, MemberOverrides::ByName)), vec!["shapes::Shape::area"]);
}

// --- B6: declared -----------------------------------------------------------------

/// `declared` reads the method's own `SUPERTYPE_OF` edges: `<Circle as
/// Loud>::speak` names `Loud::speak`, never `Quiet::speak`, though `Circle`
/// implements both traits; an unresolved edge (a placeholder) names nothing.
/// Control: the `Declared` arm of `probe` returns `None`.
#[test]
fn declared_names_exactly_the_member_the_impl_states() {
    let mut conn = setup();
    ty(&mut conn, RUST, "Circle", &["shapes", "Circle"]);
    for tr in ["Loud", "Quiet"] {
        ty(&mut conn, RUST, tr, &["shapes", tr]);
        function(&mut conn, RUST, &format!("{tr}::speak"), &["shapes", tr, "speak"]);
        supertype(&mut conn, "Circle", tr, true);
    }
    let loud = function(&mut conn, RUST, "<Circle as Loud>::speak", &["shapes", "<Circle as Loud>", "speak"]);
    let quiet =
        function(&mut conn, RUST, "<Circle as Quiet>::speak", &["shapes", "<Circle as Quiet>", "speak"]);
    supertype(&mut conn, "<Circle as Loud>::speak", "Loud::speak", true);
    supertype(&mut conn, "<Circle as Quiet>::speak", "Quiet::speak", true);

    let mut pending =
        NodeRecord::new("pending", "Function", "speak", "other::Tr::speak", "src/lib.rs", "rust");
    pending.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
    apply_diff(&mut conn, &Diff { upsert_nodes: vec![pending], ..Default::default() }).unwrap();
    let open = function(&mut conn, RUST, "<Circle as Tr>::speak", &["shapes", "<Circle as Tr>", "speak"]);
    supertype(&mut conn, "<Circle as Tr>::speak", "pending", false);

    assert_eq!(names(probe(&conn, &loud, MemberOverrides::Declared)), vec!["shapes::Loud::speak"]);
    assert_eq!(names(probe(&conn, &quiet, MemberOverrides::Declared)), vec!["shapes::Quiet::speak"]);
    assert_eq!(probe(&conn, &open, MemberOverrides::Declared), None);
}

// --- the row cap ------------------------------------------------------------------

/// A Go `T` satisfying `interfaces` interfaces that each declare `M`; the
/// anchor is `T.M`.
fn go_satisfying(interfaces: usize) -> (Connection, NodeRecord) {
    let mut conn = setup();
    let at = At { language: "go", sep: ".", file: "p/t.go", container: Some("p") };
    ty(&mut conn, at, "T", &["T"]);
    for i in 0..interfaces {
        let name = format!("I{i}");
        ty(&mut conn, at, &name, &[&name]);
        function(&mut conn, at, &format!("{name}.M"), &[&name, "M"]);
        supertype(&mut conn, "T", &name, true);
    }
    let anchor = function(&mut conn, at, "T.M", &["T", "M"]);
    (conn, anchor)
}

/// Eight rows at most; `truncated` only when more were found. Control:
/// raise `MAX_ROWS` or drop the `take` (nine rows).
#[test]
fn rows_cap_at_eight_with_truncated() {
    let (conn, anchor) = go_satisfying(9);
    let found = probe(&conn, &anchor, MemberOverrides::ByName).unwrap();
    assert_eq!(found.members.len(), 8);
    assert!(found.truncated);

    let (conn, anchor) = go_satisfying(8);
    let found = probe(&conn, &anchor, MemberOverrides::ByName).unwrap();
    assert_eq!(found.members.len(), 8);
    assert!(!found.truncated);
}
