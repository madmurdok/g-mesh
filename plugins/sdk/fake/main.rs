//! A stand-in language plugin for core's tests: it speaks the control plane
//! (handshake, `Content-Length` framing, one answer per id-carrying request)
//! and records what it was sent, and knows no language at all.
//!
//! Not shipped: nothing installs it and no bundled manifest names it. Core's
//! tests write a `plugin.toml` whose `command` is
//! `${G_MESH_BIN_DIR}/g-mesh-fake-plugin`, which resolves to the cargo profile
//! directory both a daemon binary and a unit-test binary run from.
//!
//! # Arguments
//!
//! `--language <L>` (required) and `--plugin-version <V>` (default
//! `0.0.0-test`) are what the handshake reports. `--bulk-index <root>` selects
//! the one-shot NDJSON walk instead of the control plane. Any other positional
//! argument (core appends the project root) is ignored.
//!
//! # Two personas
//!
//! **Fixture** (`--dir <plugin dir>`, `core/src/daemon/test_plugin.rs`). Every
//! observation is a file in that directory, so it survives a relaunch:
//!
//! - `spawns.log`: this process's pid, appended before anything else.
//! - `requests.log`: `"<method> <filePath>"` per id-carrying request, appended
//!   before it is answered.
//! - `notifications.log`: `"<method> <filePath>"` per notification
//!   (`filesCreated` as `"filesCreated a,b"`); never answered.
//! - `../frames.log`: `"<language> <method>"` per frame of either kind, shared
//!   by every fixture under the same plugins root.
//! - `fake-plugin.json`: options read once at start - `gated`, `stalling`,
//!   `memoryHungry`, `incompleteOnce`, `incompleteReason`,
//!   `exitWithoutHandshakeAfterMs` (a broken plugin: it says nothing, in
//!   either mode, and exits 1 after that long).
//! - `handshake.allow`: a `gated` plugin's handshake waits for it.
//! - `stalled-once.marker`: a `stalling` plugin leaves the first request this
//!   directory ever receives unanswered, then writes this marker.
//! - `incomplete-once.marker`: an `incompleteOnce` plugin answers the first
//!   `semanticPass` this directory ever receives as incomplete, then writes it.
//! - `semantic-pass.gated` / `semantic-pass.allow`: while the first exists and
//!   the second does not, `semanticPass` answers are held; other frames keep
//!   being answered meanwhile.
//! - `semantic-pass.json`: the `result` of every complete `semanticPass`.
//!
//! Every answer's `result` is `{}` unless one of the above says otherwise. The
//! bulk walk emits two nodes `<L>-n1`/`<L>-n2` and the edge `<L>-e1` between
//! them, with a signature/doc comment from `<root>/.<L>-nN.sig`/`.doc` when
//! present; `<root>/.<L>-bulk.ndjson` replaces the whole stream and
//! `<root>/.<L>-bulk.exit` sets the exit status after it.
//!
//! **Stub** (no `--dir`, `core/tests/semantic_pass_trigger.rs`). Appends each
//! received method (and `bulkIndex` for a walk) to `$G_MESH_FAKE_PLUGIN_LOG`
//! when set, answers `fileChanged`/`semanticPass` with an empty diff and any
//! other request with `{"acknowledged": true}`, and walks to one canned `File`
//! node for `seed.ts`.
//!
//! **Toy** (`--toy <defect>`, `core/tests/plugin_check.rs`): a conformant
//! plugin for the toy `.fk` language plus one deliberate defect, for
//! `g-mesh plugins check` to catch - see `toy.rs`. The first positional
//! argument is the project root it serves.
//!
//! All three exit 0 when core closes stdin.

use std::fs::{self, OpenOptions};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod toy;

use g_mesh_plugin_sdk::framing::{read_frame, write_message};
use g_mesh_plugin_sdk::wire::CURRENT_PROTOCOL_VERSION;
use serde::Deserialize;
use serde_json::{json, Map, Value};

const SPAWN_LOG: &str = "spawns.log";
const REQUEST_LOG: &str = "requests.log";
const NOTIFICATION_LOG: &str = "notifications.log";
const FRAME_LOG: &str = "frames.log";
const OPTIONS_FILE: &str = "fake-plugin.json";
const HANDSHAKE_GATE: &str = "handshake.allow";
const STALL_MARKER: &str = "stalled-once.marker";
const INCOMPLETE_MARKER: &str = "incomplete-once.marker";
const SEMANTIC_ANSWER: &str = "semantic-pass.json";
const SEMANTIC_PASS_GATED: &str = "semantic-pass.gated";
const SEMANTIC_PASS_GATE_OPEN: &str = "semantic-pass.allow";
const METHOD_LOG_ENV: &str = "G_MESH_FAKE_PLUGIN_LOG";

/// How often a gate file is checked for.
const GATE_POLL: Duration = Duration::from_millis(5);

/// The `memoryHungry` buffer: far above this process's idle footprint, held
/// for the process's whole life so a memory sample sees it.
const MEMORY_HOG_BYTES: usize = 200 * 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Options {
    gated: bool,
    stalling: bool,
    memory_hungry: bool,
    incomplete_once: bool,
    incomplete_reason: Option<String>,
    exit_without_handshake_after_ms: Option<u64>,
}

struct Args {
    language: String,
    plugin_version: String,
    dir: Option<PathBuf>,
    bulk_root: Option<PathBuf>,
    toy_defect: Option<String>,
    /// The first positional argument: core appends the project root.
    root: Option<PathBuf>,
}

type Out = Arc<Mutex<io::Stdout>>;

fn main() {
    let args = parse_args();
    if let Some(defect) = &args.toy_defect {
        toy::run(defect, args.bulk_root.as_deref(), args.root.as_deref());
        return;
    }
    match args.dir.clone() {
        Some(dir) => fixture(&args, &dir),
        None => stub(&args),
    }
}

fn parse_args() -> Args {
    let mut language = None;
    let mut plugin_version = "0.0.0-test".to_string();
    let mut dir = None;
    let mut bulk_root = None;
    let mut toy_defect = None;
    let mut root = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = |name: &str| argv.next().unwrap_or_else(|| fail(&format!("{name} needs a value")));
        match arg.as_str() {
            "--language" => language = Some(value("--language")),
            "--plugin-version" => plugin_version = value("--plugin-version"),
            "--dir" => dir = Some(PathBuf::from(value("--dir"))),
            "--bulk-index" => bulk_root = Some(PathBuf::from(value("--bulk-index"))),
            "--toy" => toy_defect = Some(value("--toy")),
            _ => {
                if root.is_none() {
                    root = Some(PathBuf::from(arg));
                }
            }
        }
    }
    let language = language.unwrap_or_else(|| fail("--language is required"));
    Args { language, plugin_version, dir, bulk_root, toy_defect, root }
}

fn fail(message: &str) -> ! {
    eprintln!("g-mesh-fake-plugin: {message}");
    process::exit(2);
}

fn append(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|err| fail(&format!("cannot open {}: {err}", path.display())));
    // One write per line: concurrent appenders never interleave a line.
    file.write_all(format!("{line}\n").as_bytes())
        .unwrap_or_else(|err| fail(&format!("cannot append to {}: {err}", path.display())));
}

fn write(out: &Out, message: &Value) {
    let mut out = out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // A closed stdout means core is gone; the stdin EOF that follows exits.
    let _ = write_message(&mut *out, message);
}

fn wait_for(path: &Path) {
    while !path.exists() {
        thread::sleep(GATE_POLL);
    }
}

fn handshake(out: &Out, args: &Args) {
    handshake_as(out, &args.language, &args.plugin_version);
}

fn handshake_as(out: &Out, language: &str, plugin_version: &str) {
    write(
        out,
        &json!({
            "protocolVersion": CURRENT_PROTOCOL_VERSION,
            "language": language,
            "pluginVersion": plugin_version,
        }),
    );
}

/// Feeds every frame on stdin to `handle`, exiting 0 at EOF (or a broken
/// stream) and 1 on a body that is not JSON.
fn serve(mut handle: impl FnMut(Value)) -> ! {
    let mut stdin = BufReader::new(io::stdin());
    loop {
        match read_frame(&mut stdin) {
            Ok(Some(body)) => match serde_json::from_slice(&body) {
                Ok(message) => handle(message),
                Err(err) => {
                    eprintln!("g-mesh-fake-plugin: frame body is not JSON: {err}");
                    process::exit(1);
                }
            },
            Ok(None) | Err(_) => process::exit(0),
        }
    }
}

fn method_of(message: &Value) -> String {
    message.get("method").and_then(Value::as_str).unwrap_or_default().to_string()
}

fn fixture(args: &Args, dir: &Path) {
    append(&dir.join(SPAWN_LOG), &process::id().to_string());
    let options: Options = match fs::read_to_string(dir.join(OPTIONS_FILE)) {
        Ok(text) => {
            serde_json::from_str(&text).unwrap_or_else(|err| fail(&format!("bad {OPTIONS_FILE}: {err}")))
        }
        Err(_) => Options::default(),
    };
    let gated = options.gated;
    let hog = options.memory_hungry.then(|| vec![1u8; MEMORY_HOG_BYTES]);
    std::hint::black_box(&hog);

    if let Some(ms) = options.exit_without_handshake_after_ms {
        thread::sleep(Duration::from_millis(ms));
        process::exit(1);
    }

    if let Some(root) = &args.bulk_root {
        process::exit(fixture_bulk(&args.language, root));
    }

    let out: Out = Arc::new(Mutex::new(io::stdout()));
    // stdin is served from the start, as it is while a gated handshake waits:
    // core closing stdin then still ends this process.
    let reader = {
        let out = Arc::clone(&out);
        let language = args.language.clone();
        let dir = dir.to_path_buf();
        thread::spawn(move || serve(|message| fixture_frame(&out, &language, &dir, &options, message)))
    };
    if gated {
        wait_for(&dir.join(HANDSHAKE_GATE));
    }
    handshake(&out, args);
    let _ = reader.join();
    drop(hog);
}

fn fixture_frame(out: &Out, language: &str, dir: &Path, options: &Options, message: Value) {
    let method = method_of(&message);
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    append(&dir.join("..").join(FRAME_LOG), &format!("{language} {method}"));

    let id = match message.get("id") {
        Some(id) if !id.is_null() => id.clone(),
        _ => {
            let paths = match params.get("filePath").and_then(Value::as_str).filter(|path| !path.is_empty()) {
                Some(path) => path.to_string(),
                None => match params.get("filePaths").and_then(Value::as_array) {
                    Some(paths) => paths
                        .iter()
                        .map(|path| path.as_str().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(","),
                    None => String::new(),
                },
            };
            append(&dir.join(NOTIFICATION_LOG), &format!("{method} {paths}"));
            return;
        }
    };

    let file_path = params.get("filePath").and_then(Value::as_str).unwrap_or_default();
    append(&dir.join(REQUEST_LOG), &format!("{method} {file_path}"));

    let stall_marker = dir.join(STALL_MARKER);
    if options.stalling && !stall_marker.exists() {
        let _ = fs::write(&stall_marker, format!("{}\n", process::id()));
        return;
    }

    let answer = {
        let out = Arc::clone(out);
        let dir = dir.to_path_buf();
        let incomplete_once = options.incomplete_once;
        let reason = options.incomplete_reason.clone();
        let method = method.clone();
        move || write(&out, &fixture_answer(&dir, &method, id, incomplete_once, reason))
    };
    if method == "semanticPass" && dir.join(SEMANTIC_PASS_GATED).exists() {
        // Held on its own thread so later frames are still answered meanwhile.
        let gate = dir.join(SEMANTIC_PASS_GATE_OPEN);
        thread::spawn(move || {
            wait_for(&gate);
            answer();
        });
    } else {
        answer();
    }
}

fn fixture_answer(
    dir: &Path,
    method: &str,
    id: Value,
    incomplete_once: bool,
    reason: Option<String>,
) -> Value {
    let incomplete_marker = dir.join(INCOMPLETE_MARKER);
    if incomplete_once && method == "semanticPass" && !incomplete_marker.exists() {
        let _ = fs::write(&incomplete_marker, format!("{}\n", process::id()));
        let mut response = json!({ "jsonrpc": "2.0", "id": id, "result": {}, "incomplete": true });
        if let Some(reason) = reason {
            response["incompleteReason"] = Value::String(reason);
        }
        return response;
    }
    if method == "semanticPass" {
        if let Ok(text) = fs::read_to_string(dir.join(SEMANTIC_ANSWER)) {
            let result: Value = serde_json::from_str(&text)
                .unwrap_or_else(|err| fail(&format!("bad {SEMANTIC_ANSWER}: {err}")));
            return json!({ "jsonrpc": "2.0", "id": id, "result": result });
        }
    }
    json!({ "jsonrpc": "2.0", "id": id, "result": {} })
}

/// The fixture's walk; returns the exit status.
fn fixture_bulk(language: &str, root: &Path) -> i32 {
    let optional = |name: String| fs::read_to_string(root.join(name)).ok();
    let mut stdout = io::stdout().lock();

    if let Some(stream) = optional(format!(".{language}-bulk.ndjson")) {
        let _ = stdout.write_all(stream.as_bytes());
        let _ = stdout.flush();
        return optional(format!(".{language}-bulk.exit"))
            .and_then(|code| code.trim().parse().ok())
            .unwrap_or(0);
    }

    let node = |n: u32, file: &str| {
        let id = format!("{language}-n{n}");
        let mut node = Map::new();
        node.insert("id".into(), json!(id));
        if let Some(signature) = optional(format!(".{id}.sig")) {
            node.insert("signature".into(), json!(signature));
        }
        if let Some(doc) = optional(format!(".{id}.doc")) {
            node.insert("docComment".into(), json!(doc));
        }
        node.insert("kind".into(), json!("Function"));
        node.insert("name".into(), json!(id));
        node.insert("qualifiedName".into(), json!(id));
        node.insert("filePath".into(), json!(format!("src/{language}-{file}.src")));
        node.insert(
            "range".into(),
            json!({ "start": { "line": 0, "col": 0 }, "end": { "line": 1, "col": 0 } }),
        );
        node.insert("visibility".into(), json!("public"));
        node.insert("language".into(), json!(language));
        Value::Object(node)
    };
    let edge = json!({
        "id": format!("{language}-e1"),
        "fromId": format!("{language}-n1"),
        "toId": format!("{language}-n2"),
        "kind": "CALLS",
        "source": "syntactic",
        "engine": "tree-sitter",
        "resolved": true,
    });
    for item in [node(1, "a"), node(2, "b"), edge] {
        let _ = writeln!(stdout, "{item}");
    }
    let _ = stdout.flush();
    0
}

fn stub(args: &Args) {
    let log = std::env::var_os(METHOD_LOG_ENV).map(PathBuf::from);
    let record = move |entry: &str| {
        if let Some(log) = &log {
            append(log, entry);
        }
    };

    if args.bulk_root.is_some() {
        record("bulkIndex");
        let node = json!({
            "id": "n1",
            "kind": "File",
            "name": "seed.ts",
            "qualifiedName": "seed.ts",
            "filePath": "seed.ts",
            "range": { "start": { "line": 0, "col": 0 }, "end": { "line": 0, "col": 0 } },
            "visibility": "file",
            "language": args.language,
        });
        let mut stdout = io::stdout().lock();
        let _ = writeln!(stdout, "{node}");
        let _ = stdout.flush();
        return;
    }

    let out: Out = Arc::new(Mutex::new(io::stdout()));
    handshake(&out, args);
    serve(|message| {
        let method = method_of(&message);
        record(&method);
        // Only an absent id is a notification here; a null one is answered.
        let Some(id) = message.get("id").cloned() else { return };
        let result = if method == "fileChanged" || method == "semanticPass" {
            json!({ "upsertNodes": [], "deleteNodeIds": [], "upsertEdges": [], "deleteEdgeIds": [] })
        } else {
            json!({ "acknowledged": true })
        };
        write(&out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    });
}
