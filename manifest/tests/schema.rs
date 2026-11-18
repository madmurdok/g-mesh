//! The schema's own contract (GM-711 D2): lenient for every reader, typed
//! where every reader agrees, raw where only one reader looks.

use g_mesh_manifest::{decode_table, parse, Value};
use serde::Deserialize;

/// The keys every reader needs, and nothing else.
const MINIMAL: &str = r#"
[plugin]
language = "toy"
protocol_version = 2
plugin_version = "1.0.0"

[plugin.spawn]
command = "./toy"

[plugin.languages]
extensions = [".toy"]
"#;

fn with(extra: &str) -> String {
    format!("{MINIMAL}\n{extra}\n")
}

/// A key or table no reader knows - the TypeScript plugin's own
/// `[plugin.grammars]`, a capability a newer core added - does not stop the
/// file parsing for the readers that do not know it.
///
/// Control: `#[serde(deny_unknown_fields)]` on `PluginSection` or
/// `CapabilitiesSection` fails this.
#[test]
fn unknown_keys_and_tables_are_accepted() {
    let text = with(
        "unknown_top = 1\n\n\
         [plugin.grammars]\ntsx = \"tree-sitter-tsx\"\n\n\
         [plugin.capabilities]\nsemantic_pass = true\nsome_future_capability = \"yes\"\n\n\
         [plugin.workspace]\nexclude_dirs = [\"vendor\"]\nfuture_list = [1, 2]\n\n\
         [plugin.spawn.extra]\nx = 1\n\n\
         [another_top_level]\nk = \"v\"\n",
    );
    let manifest = parse(&text).expect("unknown keys must not fail the parse");
    let plugin = manifest.plugin.expect("[plugin] present");
    assert_eq!(plugin.language.as_deref(), Some("toy"));
    assert_eq!(plugin.capabilities.semantic_pass, Some(true));
    assert_eq!(plugin.workspace.exclude_dirs, vec!["vendor"]);
}

/// Every section is optional here: requiring one is the reader's rule.
#[test]
fn missing_sections_are_none_or_their_default() {
    let empty = parse("").expect("an empty file is a manifest with nothing in it");
    assert_eq!(empty.plugin, None);

    let plugin = parse("[plugin]\n").unwrap().plugin.expect("an empty [plugin] is present");
    assert_eq!(plugin.language, None);
    assert_eq!(plugin.protocol_version, None);
    assert_eq!(plugin.plugin_version, None);
    assert_eq!(plugin.spawn, None);
    assert_eq!(plugin.languages, None);
    assert!(plugin.fingerprint.ignore.is_empty());
    assert_eq!(plugin.capabilities, Default::default());
    assert_eq!(plugin.workspace, Default::default());
    assert_eq!(plugin.semantic, None);
    assert_eq!(plugin.non_symbol_queries, None);
    assert_eq!(plugin.symbol_query_prefixes, None);
    assert_eq!(plugin.reexports, None);
    assert_eq!(plugin.semantic_section().unwrap(), None);

    // A table present with its keys missing: `None`, or empty lists.
    let plugin = parse("[plugin.spawn]\n[plugin.languages]\n[plugin.workspace]\n[plugin.capabilities]\n")
        .unwrap()
        .plugin
        .unwrap();
    let spawn = plugin.spawn.expect("[plugin.spawn] present");
    assert_eq!(spawn.command, None);
    assert!(spawn.args.is_empty());
    assert_eq!(plugin.languages.expect("[plugin.languages] present").extensions, None);
    assert!(plugin.workspace.watch_files.is_empty());
    assert!(plugin.workspace.exclude_dirs.is_empty());
    assert!(plugin.workspace.entry_points.is_empty());
    assert_eq!(plugin.capabilities.receiver_calls, None);
}

/// A key every reader types the same, written with the wrong type, fails the
/// parse for all of them.
#[test]
fn a_known_key_of_the_wrong_type_fails_the_parse() {
    for (tag, text) in [
        ("capability", with("[plugin.capabilities]\nsemantic_pass = \"yes\"\n")),
        ("extensions", MINIMAL.replace("extensions = [\".toy\"]", "extensions = \".toy\"")),
        ("protocol_version", MINIMAL.replace("protocol_version = 2", "protocol_version = \"2\"")),
        ("exclude_dirs", with("[plugin.workspace]\nexclude_dirs = \"vendor\"\n")),
    ] {
        assert!(parse(&text).is_err(), "{tag}: a wrong-typed known key must fail the parse");
    }
}

/// `[plugin.semantic]` is the LSP bridge's alone: a wrong-typed key in it is
/// the bridge's error (`semantic_section`), never the parse's - so core,
/// which never looks at the table, still discovers the plugin.
///
/// Control: type the field (`semantic: Option<SemanticSection>`) and this
/// fails at `parse`.
#[test]
fn a_wrong_typed_semantic_key_parses_and_fails_only_in_semantic_section() {
    let text = with("[plugin.semantic]\ncommand = \"server\"\nargs = \"--stdio\"\n");
    let plugin = parse(&text).expect("a broken [plugin.semantic] must not fail the parse").plugin.unwrap();
    assert!(plugin.semantic.is_some(), "the table is kept raw");
    assert!(plugin.semantic_section().is_err(), "the bridge's decode reports it");
}

#[test]
fn a_well_formed_semantic_table_decodes_every_key() {
    let text = with(
        "[plugin.semantic]\ncommand = \"server\"\nargs = [\"--stdio\"]\nengine = \"srv\"\n\
         implementation_kinds = [\"trait\"]\nreadiness = \"on-demand\"\n\
         overload_disambiguation = \"hover\"\n\
         [plugin.semantic.env]\nA = \"1\"\n\
         [plugin.semantic.settings.python.analysis]\ntypeCheckingMode = \"basic\"\n",
    );
    let semantic = parse(&text).unwrap().plugin.unwrap().semantic_section().unwrap().expect("present");
    assert_eq!(semantic.command.as_deref(), Some("server"));
    assert_eq!(semantic.args, vec!["--stdio"]);
    assert_eq!(semantic.engine.as_deref(), Some("srv"));
    assert_eq!(semantic.implementation_kinds, vec!["trait"]);
    assert_eq!(semantic.readiness.as_deref(), Some("on-demand"));
    assert_eq!(semantic.overload_disambiguation.as_deref(), Some("hover"));
    assert_eq!(semantic.env.get("A").map(String::as_str), Some("1"));
    let settings = semantic.settings.expect("settings kept");
    assert_eq!(settings["python"]["analysis"]["typeCheckingMode"].as_str(), Some("basic"));
    assert_eq!(semantic.initialization_options, None);
}

/// A semantic table without `command` still parses and decodes: requiring
/// it is the bridge's rule, with the bridge's message.
#[test]
fn a_semantic_table_without_command_decodes_with_command_none() {
    let text = with("[plugin.semantic]\nargs = [\"--stdio\"]\n");
    let semantic = parse(&text).unwrap().plugin.unwrap().semantic_section().unwrap().expect("present");
    assert_eq!(semantic.command, None);
}

/// A stand-in for one of core's strict tables.
#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Strict {
    #[serde(default)]
    starts_with: Vec<String>,
}

/// Core's tables stay raw through the parse, whatever is in them: a typo or a
/// wrong type there is core's error, with core's types.
#[test]
fn core_only_tables_are_kept_raw_whatever_they_hold() {
    let text = with(
        "[plugin.non_symbol_queries]\nstart_with = [\"@\"]\nstarts_with = 5\n\n\
         [plugin.symbol_query_prefixes]\nstrip = \"@\"\n\n\
         [plugin.reexports]\nunknown_rule = true\n",
    );
    let plugin = parse(&text).expect("core-only tables are not typed by the schema").plugin.unwrap();
    let queries = plugin.non_symbol_queries.expect("kept");
    assert_eq!(queries["starts_with"].as_integer(), Some(5));
    assert_eq!(queries["start_with"][0].as_str(), Some("@"));
    assert_eq!(plugin.symbol_query_prefixes.expect("kept")["strip"].as_str(), Some("@"));
    assert_eq!(plugin.reexports.expect("kept")["unknown_rule"].as_bool(), Some(true));
}

#[test]
fn decode_table_of_an_absent_table_is_the_default() {
    let decoded: Strict = decode_table(None).unwrap();
    assert_eq!(decoded, Strict::default());
}

#[test]
fn decode_table_decodes_a_well_formed_table() {
    let table: Value = toml::from_str("starts_with = [\"@\", \".\"]").unwrap();
    let decoded: Strict = decode_table(Some(&table)).unwrap();
    assert_eq!(decoded.starts_with, vec!["@", "."]);
}

/// A strict type stays strict through `decode_table`: an unknown key and a
/// wrong type are both errors naming the key.
///
/// Control: make `decode_table` fall back to `T::default()` on an error.
#[test]
fn decode_table_keeps_a_strict_type_strict() {
    let unknown: Value = toml::from_str("start_with = [\"@\"]").unwrap();
    let err = decode_table::<Strict>(Some(&unknown)).expect_err("an unknown key must be rejected");
    assert!(err.to_string().contains("start_with"), "{err}");

    let wrong_type: Value = toml::from_str("starts_with = 5").unwrap();
    assert!(decode_table::<Strict>(Some(&wrong_type)).is_err(), "a wrong type must be rejected");
}
