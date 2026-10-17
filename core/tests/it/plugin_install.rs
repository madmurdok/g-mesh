//! `g-mesh plugins install` / `g-mesh plugins remove`, driven through the real
//! binary against a throwaway plugin root.
//!
//! Every run points `G_MESH_PLUGIN_ROOTS_OVERRIDE` at a temporary directory,
//! so nothing lands beside the test binary, and `G_MESH_DOWNLOAD_BASE` at a
//! local HTTP stand-in, so no test reaches the network. Whether an install
//! worked is judged by `daemon::manifest::discover` over that root - the
//! function the daemon itself starts from - rather than by `plugins list`.
//!
//! "Nothing installed" is asserted as "the root's tree is exactly what it was
//! before", which also catches a staging directory or a partial download left
//! behind.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use g_mesh::daemon::manifest;
use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const LANGUAGE: &str = "toylang";
/// Printed by both commands; the full sentence lives in `plugin_install.rs`.
const RESTART_HINT_FRAGMENT: &str = "only discovers plugins when it starts: run `g-mesh stop`";

fn plugin_toml(language: &str, plugin_version: &str) -> String {
    format!(
        r#"
[plugin]
language = "{language}"
protocol_version = {protocol}
plugin_version = "{plugin_version}"

[plugin.spawn]
command = "node"

[plugin.languages]
extensions = [".{language}"]
"#,
        protocol = g_mesh::protocol::types::CURRENT_PROTOCOL_VERSION,
    )
}

/// A `.tar.gz` holding `entries` (path inside the archive, contents); the
/// directories are implied by the paths.
fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, path, *contents).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// A well-formed plugin archive for `language` at `plugin_version`.
fn plugin_archive(language: &str, plugin_version: &str) -> Vec<u8> {
    let manifest = plugin_toml(language, plugin_version);
    tar_gz(&[
        (&format!("{language}/plugin.toml"), manifest.as_bytes()),
        (&format!("{language}/lib/data.txt"), b"plugin payload\n"),
    ])
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A local stand-in for the release host: answers `*.sha256` with
/// `checksum` and `*.tar.gz` with `archive`, anything else with 404, and
/// records every request path it was sent. The listener thread outlives the
/// test; it only ever blocks in `accept`.
struct Server {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(checksum: Vec<u8>, archive: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind a test server");
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { continue };
                let path = read_request_path(&mut socket);
                seen.lock().unwrap().push(path.clone());
                let body = if path.ends_with(".tar.gz.sha256") {
                    Some(&checksum)
                } else if path.ends_with(".tar.gz") {
                    Some(&archive)
                } else {
                    None
                };
                let _ = match body {
                    Some(body) => write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .and_then(|()| socket.write_all(body)),
                    None => write!(
                        socket,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    ),
                };
                let _ = socket.flush();
            }
        });
        Self { base: format!("http://127.0.0.1:{port}"), requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// Reads up to the end of the request head and returns the request path.
fn read_request_path(socket: &mut std::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&head);
    head.split_whitespace().nth(1).unwrap_or("").to_string()
}

/// Runs `g-mesh plugins <args>` with `root` as the only plugin root and
/// `download_base` as the release host. `HOME` is a scratch directory so
/// nothing reads or writes the real `~/.g-mesh`.
fn run(args: &[&str], root: &Path, download_base: &str, home: &Path) -> Output {
    Command::new(BIN)
        .arg("plugins")
        .args(args)
        .env(manifest::PLUGIN_ROOTS_OVERRIDE_ENV, root)
        .env("G_MESH_DOWNLOAD_BASE", download_base)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("failed to run g-mesh")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Every file and directory under `root`, relative to it, with a file's
/// contents so a rewrite would show too. An absent root lists as empty.
fn tree(root: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeSet<String>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                found.insert(format!("{relative}/"));
                walk(root, &path, found);
            } else {
                let contents = fs::read(&path).unwrap();
                found.insert(format!("{relative} = {}", sha256_hex(&contents)));
            }
        }
    }
    let mut found = BTreeSet::new();
    walk(root, root, &mut found);
    found
}

/// A port nothing listens on: bound, then released.
fn dead_base() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

struct Scratch {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    files: PathBuf,
}

fn scratch() -> Scratch {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("plugins");
    let home = dir.path().join("home");
    let files = dir.path().join("files");
    for path in [&root, &home, &files] {
        fs::create_dir_all(path).unwrap();
    }
    Scratch { _dir: dir, root, home, files }
}

/// The plugin discovery would load from `root`, if any, as the daemon sees it.
fn discovered(root: &Path, language: &str) -> Option<manifest::PluginManifest> {
    manifest::discover(&[root.to_path_buf()]).unwrap().manifests.remove(language)
}

// ---- install <language>: the release path -------------------------------

#[test]
fn a_release_download_that_does_not_match_its_published_digest_installs_nothing() {
    let s = scratch();
    let published = plugin_archive(LANGUAGE, "1.0.0");
    let tampered = plugin_archive(LANGUAGE, "6.6.6");
    let expected = sha256_hex(&published);
    let actual = sha256_hex(&tampered);
    assert_ne!(expected, actual);
    let server = Server::start(format!("{expected}  asset.tar.gz\n").into_bytes(), tampered);

    let output = run(&["install", LANGUAGE], &s.root, &server.base, &s.home);

    let err = stderr(&output);
    assert!(!output.status.success(), "a mismatching download must fail; stdout: {}", stdout(&output));
    assert!(err.contains(&expected), "the published digest is named: {err}");
    assert!(err.contains(&actual), "the digest of what arrived is named: {err}");
    assert!(err.contains(&format!("release v{VERSION}")), "the release is named: {err}");
    assert_eq!(
        tree(&s.root),
        BTreeSet::new(),
        "no plugin, staging directory or partial download is left in the root"
    );
    assert!(discovered(&s.root, LANGUAGE).is_none());
}

#[test]
fn a_release_download_that_matches_its_digest_lands_where_discovery_reads() {
    let s = scratch();
    let archive = plugin_archive(LANGUAGE, "1.2.3");
    let digest = sha256_hex(&archive);
    let server = Server::start(format!("{digest}  asset.tar.gz\n").into_bytes(), archive);

    let output = run(&["install", LANGUAGE], &s.root, &server.base, &s.home);

    assert!(output.status.success(), "install failed: {}", stderr(&output));
    let found = discovered(&s.root, LANGUAGE).expect("discovery finds the installed plugin");
    assert_eq!(found.plugin_version, "1.2.3");
    assert_eq!(found.manifest_dir, s.root.join(LANGUAGE));
    assert_eq!(fs::read(s.root.join(LANGUAGE).join("lib").join("data.txt")).unwrap(), b"plugin payload\n");
    let leftovers: Vec<_> =
        tree(&s.root).into_iter().filter(|path| !path.starts_with(&format!("{LANGUAGE}/"))).collect();
    assert!(leftovers.is_empty(), "only the plugin's directory is added: {leftovers:?}");
    assert!(stdout(&output).contains(RESTART_HINT_FRAGMENT), "stdout: {}", stdout(&output));

    let asset_prefix = format!("/v{VERSION}/g-mesh-plugin-{LANGUAGE}-v{VERSION}-");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the checksum and the archive, nothing else: {requests:?}");
    assert!(
        requests.iter().all(|path| path.starts_with(&asset_prefix)),
        "both come from this version's release asset: {requests:?}"
    );
    assert!(requests.iter().any(|path| path.ends_with(".tar.gz.sha256")), "{requests:?}");
    assert!(requests.iter().any(|path| path.ends_with(".tar.gz")), "{requests:?}");
}

#[test]
fn a_release_archive_for_another_language_is_refused() {
    let s = scratch();
    let archive = plugin_archive("other", "1.0.0");
    let digest = sha256_hex(&archive);
    let server = Server::start(format!("{digest}  asset.tar.gz\n").into_bytes(), archive);

    let output = run(&["install", LANGUAGE], &s.root, &server.base, &s.home);

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(stderr(&output).contains("not toylang/"), "stderr: {}", stderr(&output));
    assert_eq!(tree(&s.root), BTreeSet::new(), "nothing is installed under either name");
}

// ---- install --from ------------------------------------------------------

#[test]
fn from_an_archive_without_a_sidecar_installs_unverified_and_opens_no_connection() {
    let s = scratch();
    let archive = s.files.join("plugin.tar.gz");
    fs::write(&archive, plugin_archive(LANGUAGE, "2.0.0")).unwrap();
    // Anything the command fetched would be recorded here.
    let server = Server::start(Vec::new(), Vec::new());

    let output = run(&["install", "--from", archive.to_str().unwrap()], &s.root, &server.base, &s.home);

    assert!(output.status.success(), "install failed: {}", stderr(&output));
    assert!(stdout(&output).contains("no checksum available"), "stdout: {}", stdout(&output));
    assert!(stdout(&output).contains(RESTART_HINT_FRAGMENT), "stdout: {}", stdout(&output));
    assert_eq!(discovered(&s.root, LANGUAGE).expect("installed").plugin_version, "2.0.0");
    assert_eq!(server.requests(), Vec::<String>::new(), "--from never reaches the release host");
}

#[test]
fn from_an_archive_with_a_matching_sidecar_installs_verified() {
    let s = scratch();
    let bytes = plugin_archive(LANGUAGE, "2.1.0");
    let archive = s.files.join("plugin.tar.gz");
    fs::write(&archive, &bytes).unwrap();
    fs::write(s.files.join("plugin.tar.gz.sha256"), format!("{}  plugin.tar.gz\n", sha256_hex(&bytes)))
        .unwrap();

    let output = run(&["install", "--from", archive.to_str().unwrap()], &s.root, &dead_base(), &s.home);

    assert!(output.status.success(), "install failed: {}", stderr(&output));
    assert!(stdout(&output).contains("checksum ok"), "stdout: {}", stdout(&output));
    assert_eq!(discovered(&s.root, LANGUAGE).expect("installed").plugin_version, "2.1.0");
}

#[test]
fn from_an_archive_whose_sidecar_disagrees_installs_nothing() {
    let s = scratch();
    // An existing plugin in the root must survive the refusal untouched.
    let installed = s.root.join("other");
    fs::create_dir_all(&installed).unwrap();
    fs::write(installed.join("plugin.toml"), plugin_toml("other", "0.1.0")).unwrap();
    let before = tree(&s.root);

    let bytes = plugin_archive(LANGUAGE, "2.2.0");
    let archive = s.files.join("plugin.tar.gz");
    fs::write(&archive, &bytes).unwrap();
    let wrong = sha256_hex(b"some other archive");
    fs::write(s.files.join("plugin.tar.gz.sha256"), format!("{wrong}  plugin.tar.gz\n")).unwrap();

    let output = run(&["install", "--from", archive.to_str().unwrap()], &s.root, &dead_base(), &s.home);

    let err = stderr(&output);
    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(err.contains(&wrong), "the sidecar's digest is named: {err}");
    assert!(err.contains(&sha256_hex(&bytes)), "the archive's digest is named: {err}");
    assert_eq!(tree(&s.root), before);
}

#[test]
fn from_an_archive_whose_sidecar_is_not_a_digest_installs_nothing() {
    let s = scratch();
    let archive = s.files.join("plugin.tar.gz");
    fs::write(&archive, plugin_archive(LANGUAGE, "2.3.0")).unwrap();
    fs::write(s.files.join("plugin.tar.gz.sha256"), "not-a-digest  plugin.tar.gz\n").unwrap();

    let output = run(&["install", "--from", archive.to_str().unwrap()], &s.root, &dead_base(), &s.home);

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(stderr(&output).contains("malformed"), "stderr: {}", stderr(&output));
    assert_eq!(tree(&s.root), BTreeSet::new());
}

#[test]
fn from_a_plugin_directory_installs_a_copy_of_it() {
    let s = scratch();
    let source = s.files.join(LANGUAGE);
    fs::create_dir_all(source.join("lib")).unwrap();
    fs::write(source.join("plugin.toml"), plugin_toml(LANGUAGE, "3.0.0")).unwrap();
    fs::write(source.join("lib").join("data.txt"), "from a directory\n").unwrap();
    let source_before = tree(&source);

    let output = run(&["install", "--from", source.to_str().unwrap()], &s.root, &dead_base(), &s.home);

    assert!(output.status.success(), "install failed: {}", stderr(&output));
    assert_eq!(discovered(&s.root, LANGUAGE).expect("installed").plugin_version, "3.0.0");
    assert_eq!(tree(&s.root.join(LANGUAGE)), source_before, "the installed copy matches its source");
    assert_eq!(tree(&source), source_before, "the source directory is copied, not moved");
}

// ---- layout refusals -----------------------------------------------------

#[test]
fn an_archive_with_more_than_one_top_level_entry_installs_nothing() {
    let s = scratch();
    let manifest = plugin_toml(LANGUAGE, "4.0.0");
    let archive = s.files.join("plugin.tar.gz");
    fs::write(
        &archive,
        tar_gz(&[
            (&format!("{LANGUAGE}/plugin.toml"), manifest.as_bytes()),
            ("extra/stray.txt", b"not part of the plugin\n"),
        ]),
    )
    .unwrap();

    let output = run(&["install", "--from", archive.to_str().unwrap()], &s.root, &dead_base(), &s.home);

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(stderr(&output).contains("more than one top-level entry"), "stderr: {}", stderr(&output));
    assert_eq!(tree(&s.root), BTreeSet::new());
}

#[test]
fn an_archive_without_a_plugin_toml_installs_nothing() {
    let s = scratch();
    let archive = s.files.join("plugin.tar.gz");
    fs::write(&archive, tar_gz(&[(&format!("{LANGUAGE}/README.txt"), b"no manifest here\n")])).unwrap();

    let output = run(&["install", "--from", archive.to_str().unwrap()], &s.root, &dead_base(), &s.home);

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(stderr(&output).contains("plugin.toml"), "stderr: {}", stderr(&output));
    assert_eq!(tree(&s.root), BTreeSet::new(), "no plugin and no staging directory is left");
}

// ---- remove --------------------------------------------------------------

/// A root holding `toylang` and `other` plugins plus a loose file and a
/// hidden directory, all of which a remove of `toylang` must leave alone.
fn populated_root(root: &Path) {
    for (language, version) in [(LANGUAGE, "1.0.0"), ("other", "5.0.0")] {
        let dir = root.join(language);
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(dir.join("plugin.toml"), plugin_toml(language, version)).unwrap();
        fs::write(dir.join("lib").join("data.txt"), format!("{language} payload\n")).unwrap();
    }
    fs::write(root.join("notes.txt"), "a loose file\n").unwrap();
    fs::create_dir_all(root.join(".hidden")).unwrap();
    fs::write(root.join(".hidden").join("keep.txt"), "hidden\n").unwrap();
}

#[test]
fn remove_deletes_that_plugin_directory_and_nothing_else() {
    let s = scratch();
    populated_root(&s.root);
    let before = tree(&s.root);

    let output = run(&["remove", LANGUAGE], &s.root, &dead_base(), &s.home);

    assert!(output.status.success(), "remove failed: {}", stderr(&output));
    let expected: BTreeSet<String> =
        before.into_iter().filter(|path| !path.starts_with(&format!("{LANGUAGE}/"))).collect();
    assert_eq!(tree(&s.root), expected);
    assert!(discovered(&s.root, LANGUAGE).is_none());
    assert!(discovered(&s.root, "other").is_some());
    assert!(stdout(&output).contains(RESTART_HINT_FRAGMENT), "stdout: {}", stdout(&output));
}

#[test]
fn remove_of_a_plugin_that_is_not_installed_refuses_and_changes_nothing() {
    let s = scratch();
    populated_root(&s.root);
    fs::remove_dir_all(s.root.join(LANGUAGE)).unwrap();
    let before = tree(&s.root);

    let output = run(&["remove", LANGUAGE], &s.root, &dead_base(), &s.home);

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert!(stderr(&output).contains("no toylang plugin is installed"), "stderr: {}", stderr(&output));
    assert_eq!(tree(&s.root), before);
}

#[test]
fn remove_refuses_a_name_that_addresses_another_directory() {
    let s = scratch();
    populated_root(&s.root);
    // `root/..` is the scratch directory: everything in it must survive.
    let outer = s.root.parent().unwrap().to_path_buf();
    let before = tree(&outer);

    let output = run(&["remove", ".."], &s.root, &dead_base(), &s.home);

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    assert_eq!(tree(&outer), before);
}

// ---- who can call it -----------------------------------------------------

/// Installing and removing plugins is something only a person runs. The
/// module is private to the CLI's dispatch: no source file other than itself
/// and `cli/mod.rs` (which routes `g-mesh plugins install|remove` to it)
/// names it, so neither the daemon nor any MCP handler can reach it. Its
/// downloads go through `cli::model`'s `pub(super)` helpers, which nothing
/// outside `cli` can call at all. Asserted over the source tree because the
/// change it guards against - "the plugin is missing, let me install it" in
/// discovery - would look helpful in review.
#[test]
fn nothing_but_the_cli_dispatch_names_the_install_module() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let allowed = [src.join("cli").join("mod.rs"), src.join("cli").join("plugin_install.rs")];

    let mut offenders = Vec::new();
    for path in rust_sources(&src) {
        if allowed.contains(&path) {
            continue;
        }
        if names_identifier(&fs::read_to_string(&path).unwrap(), "plugin_install") {
            offenders.push(path);
        }
    }

    assert!(
        offenders.is_empty(),
        "only cli/mod.rs may dispatch to cli::plugin_install; found it named in {offenders:?}. \
         g-mesh installs or removes a plugin only when a person runs `g-mesh plugins install|remove`."
    );
}

/// Whether `text` contains `ident` as a whole identifier (so
/// `no_plugin_installed` does not count as `plugin_install`).
fn names_identifier(text: &str, ident: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(ident).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + ident.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found
}
