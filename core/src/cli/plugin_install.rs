//! `g-mesh plugins install` / `g-mesh plugins remove`: adding and deleting a
//! language plugin beside the running g-mesh binary.
//!
//! Plugins land in `<dir of the g-mesh executable>/plugins/<language>/`, the
//! installed bundled root [`crate::daemon::manifest::discover`] scans, so a
//! plugin is version-locked to the core that installed it and replaced by the
//! next `install.sh`. With [`manifest::PLUGIN_ROOTS_OVERRIDE_ENV`] set, that
//! one directory is the target instead, the same directory discovery would
//! then read.
//!
//! # Where the bytes come from
//!
//! - `install <language>` downloads `g-mesh-plugin-<language>-v<version>-<target>.tar.gz`
//!   and its `.sha256` from this binary's own release (layout:
//!   `docs/architecture/plugin-distribution.md`, "Per-plugin release
//!   assets"), through [`crate::cli::model`] - the crate's only HTTP client.
//!   The archive's digest is compared with the published one before anything
//!   is unpacked; a mismatch deletes the download, installs nothing and
//!   prints both digests. The same rule is implemented in
//!   `scripts/install.sh` and `scripts/install.ps1`; change all three
//!   ([ADR 0027](../../../docs/adr/0027-plugin-fetch-checksums-in-rust.md)).
//! - `install --from <path>` takes a local `.tar.gz` or an unpacked plugin
//!   directory and never opens a network connection. An `<archive>.sha256`
//!   beside the archive is checked with the same rule; without one the
//!   install goes ahead and says it was not verified.
//!
//! # Nothing half-installed
//!
//! Everything is unpacked or copied into a staging directory inside the
//! target root, validated there (one top-level `<language>/` directory, a
//! `plugin.toml` that parses and names that language, its spawn command
//! present), and only then renamed into place. A failure at any step leaves
//! the root as it was; a plugin already installed under that language is
//! moved back if the final rename fails.
//!
//! Only a human runs these: nothing under `daemon` can call into `cli` (the
//! dependency runs the other way), so discovery never installs or removes a
//! plugin by itself.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Args};
use sha2::{Digest, Sha256};

use crate::cli::model;
use crate::daemon::manifest;

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("source").required(true).args(["language", "from"])))]
pub struct InstallArgs {
    /// The plugin's language (e.g. `python`), downloaded from the GitHub
    /// release matching this g-mesh's version and platform.
    pub language: Option<String>,
    /// A local plugin archive (`.tar.gz`) or an unpacked plugin directory (the
    /// one holding `plugin.toml`, named after its language). No network access.
    #[arg(long, value_name = "PATH")]
    pub from: Option<PathBuf>,
}

/// `<root>/<language>/plugin.toml`, the file discovery looks for.
const MANIFEST_FILE_NAME: &str = "plugin.toml";

/// Base for `<version-tag>/<asset>` URLs, the same variable and meaning as in
/// `scripts/install.sh`.
const DOWNLOAD_BASE_ENV: &str = "G_MESH_DOWNLOAD_BASE";

/// `owner/repo` the default download base points at, as in `scripts/install.sh`.
const REPO_ENV: &str = "G_MESH_REPO";
const DEFAULT_REPO: &str = "madmurdok/g-mesh";

/// Printed after every successful install and remove: discovery runs once, at
/// daemon startup.
const RESTART_HINT: &str =
    "A running daemon only discovers plugins when it starts: run `g-mesh stop` in each \
     project whose daemon is up (`g-mesh status` shows it); the next query starts a fresh one.";

/// Runs `g-mesh plugins install`.
pub fn install(args: &InstallArgs) -> Result<()> {
    let root = install_root()?;
    let mut out = io::stdout().lock();
    match (&args.from, &args.language) {
        (Some(path), _) => install_from(path, &root, &mut out),
        (None, Some(language)) => {
            let target = release_target().context(
                "no plugin assets are published for this platform; install from a local archive \
                 or directory with `g-mesh plugins install --from <path>`",
            )?;
            install_release(language, &root, &download_base(), env!("CARGO_PKG_VERSION"), target, &mut out)
        }
        (None, None) => bail!("name a language, or a local plugin with --from <path>"),
    }
}

/// Runs `g-mesh plugins remove <language>`.
pub fn remove(language: &str) -> Result<()> {
    remove_from(language, &install_root()?, &mut io::stdout().lock())
}

/// The root plugins are installed into and removed from.
fn install_root() -> Result<PathBuf> {
    install_root_from(std::env::var_os(manifest::PLUGIN_ROOTS_OVERRIDE_ENV).map(PathBuf::from))
}

/// The override replaces the root exactly as it replaces discovery's roots
/// (`manifest::default_roots`), so an install lands where discovery reads.
fn install_root_from(override_root: Option<PathBuf>) -> Result<PathBuf> {
    match override_root {
        Some(root) => Ok(root),
        None => manifest::installed_bundled_root()
            .context("could not locate the g-mesh executable, so there is no plugins directory beside it"),
    }
}

fn download_base() -> String {
    let non_empty = |name| std::env::var(name).ok().filter(|value: &String| !value.is_empty());
    non_empty(DOWNLOAD_BASE_ENV).unwrap_or_else(|| {
        let repo = non_empty(REPO_ENV).unwrap_or_else(|| DEFAULT_REPO.to_string());
        format!("https://github.com/{repo}/releases/download")
    })
}

/// The Rust target triple this binary's release assets are named after, or
/// `None` on a platform no release is built for. The four triples are
/// `scripts/build-targets.sh --list`.
fn release_target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc")) {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// `g-mesh-plugin-<language>-v<version>-<target>.tar.gz`, as
/// `scripts/build-targets.sh`'s `plugin_asset_name_for` writes it.
fn asset_name(language: &str, version: &str, target: &str) -> String {
    format!("g-mesh-plugin-{language}-v{version}-{target}.tar.gz")
}

fn install_release(
    language: &str,
    root: &Path,
    base: &str,
    version: &str,
    target: &str,
    out: &mut impl Write,
) -> Result<()> {
    validate_language(language)?;
    let asset = asset_name(language, version, target);
    let release = format!("v{version}");
    let url = format!("{}/{release}/{asset}", base.trim_end_matches('/'));

    let staging = Staging::create(root, language)?;
    writeln!(out, "downloading {asset} from release {release}")?;
    let checksum = model::fetch_text(&format!("{url}.sha256")).with_context(|| {
        format!(
            "could not download {asset}.sha256, so the plugin cannot be verified; release \
             {release} may not be published yet, or may not carry a {language} plugin for {target} \
             - nothing was installed"
        )
    })?;
    let expected = parse_checksum(&checksum, &format!("{asset}.sha256"))?;

    let archive = staging.path().join(&asset);
    let actual = model::fetch_to_file(&url, &archive).with_context(|| {
        format!(
            "could not download {asset}; release {release} may not be published yet, or may not \
             carry a {language} plugin for {target} - nothing was installed"
        )
    })?;
    if actual != expected {
        let _ = fs::remove_file(&archive);
        bail!(
            "checksum mismatch for {asset} from release {release} - nothing was installed.\n  \
             expected: {expected}\n  actual:   {actual}\n\
             The download is corrupt or has been tampered with; retry, and report it if it keeps failing."
        );
    }
    writeln!(out, "checksum ok")?;

    let staged = unpack(&archive, Some(language), &staging)?;
    place(&staged, root, &staging, out)
}

fn install_from(path: &Path, root: &Path, out: &mut impl Write) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if metadata.is_dir() {
        let language = directory_language(path)?;
        let staging = Staging::create(root, &language)?;
        let staged = staging.path().join("unpack").join(&language);
        copy_tree(path, &staged)?;
        validate_plugin_dir(&staged, &language, &path.display().to_string())?;
        return place(&staged, root, &staging, out);
    }

    let sidecar = sidecar_path(path);
    let expected = match fs::read_to_string(&sidecar) {
        Ok(text) => Some(parse_checksum(&text, &sidecar.display().to_string())?),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", sidecar.display())),
    };
    match &expected {
        Some(expected) => {
            let actual = sha256_of_file(path)?;
            if &actual != expected {
                bail!(
                    "checksum mismatch for {} against {} - nothing was installed.\n  \
                     expected: {expected}\n  actual:   {actual}",
                    path.display(),
                    sidecar.display()
                );
            }
            writeln!(out, "checksum ok ({})", sidecar.display())?;
        }
        None => writeln!(
            out,
            "no checksum available ({} does not exist); installing without verification",
            sidecar.display()
        )?,
    }

    let language = archive_language(path)?;
    let staging = Staging::create(root, &language)?;
    let staged = unpack(path, Some(&language), &staging)?;
    place(&staged, root, &staging, out)
}

fn remove_from(language: &str, root: &Path, out: &mut impl Write) -> Result<()> {
    validate_language(language)?;
    let dir = root.join(language);
    match fs::symlink_metadata(&dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => bail!(
            "no {language} plugin is installed in {}; nothing was removed (`g-mesh plugins list` \
             shows what is installed and where)",
            root.display()
        ),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dir.display())),
        Ok(metadata) if !metadata.is_dir() => {
            bail!("{} is not a plugin directory; nothing was removed", dir.display())
        }
        Ok(_) => {}
    }
    // Discovery is filesystem-based (`manifest::discover` scans the roots for
    // `<language>/plugin.toml`), so deleting the directory is the whole
    // removal: there is no config entry to edit.
    fs::remove_dir_all(&dir).with_context(|| format!("failed to remove {}", dir.display()))?;
    writeln!(out, "removed the {language} plugin from {}", dir.display())?;
    writeln!(out, "{RESTART_HINT}")?;
    Ok(())
}

/// A language name doubles as a directory name under the root, so anything
/// that could address another path (`..`, separators) is refused.
fn validate_language(language: &str) -> Result<()> {
    let valid = !language.is_empty()
        && !language.starts_with('.')
        && language.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!("\"{language}\" is not a plugin language name (letters, digits, `-` and `_` only)");
    }
    Ok(())
}

/// The digest from a `.sha256` file, `<hex>  <basename>` as
/// `scripts/build-targets.sh` writes it; only the digest is read.
fn parse_checksum(text: &str, name: &str) -> Result<String> {
    let digest = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("the checksum file {name} is empty or malformed - refusing to install unverified bytes");
    }
    Ok(digest)
}

/// `<archive>.sha256`, the name a release gives an asset's checksum.
fn sidecar_path(archive: &Path) -> PathBuf {
    let mut name = archive.as_os_str().to_owned();
    name.push(".sha256");
    PathBuf::from(name)
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).with_context(|| format!("failed while reading {}", path.display()))?;
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The language an unpacked plugin directory provides: its own name, which
/// `manifest::read_manifest` requires to match the manifest's language.
fn directory_language(dir: &Path) -> Result<String> {
    if !dir.join(MANIFEST_FILE_NAME).is_file() {
        bail!(
            "{} has no {MANIFEST_FILE_NAME}; pass the plugin's own directory (the one named after \
             its language) or a plugin archive",
            dir.display()
        );
    }
    let canonical = fs::canonicalize(dir).with_context(|| format!("cannot resolve {}", dir.display()))?;
    let language = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("cannot tell which language {} is for", dir.display()))?
        .to_string();
    validate_language(&language)?;
    Ok(language)
}

/// The single top-level directory of a plugin archive, which names its
/// language. Refuses anything that would unpack outside that directory or is
/// not a plain file or directory.
fn archive_language(archive: &Path) -> Result<String> {
    let mut top: Option<String> = None;
    for_each_entry(archive, |path, entry| {
        let first = archive_top_level(path, archive)?;
        match &top {
            None => top = Some(first),
            Some(seen) if *seen != first => bail!(
                "{} is not a plugin archive: it has more than one top-level entry ({seen}, {first}); \
                 a plugin archive holds a single <language>/ directory - nothing was installed",
                archive.display()
            ),
            Some(_) => {}
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            bail!(
                "{} contains {}, which is neither a file nor a directory - nothing was installed",
                archive.display(),
                path.display()
            );
        }
        Ok(())
    })?;
    let language =
        top.with_context(|| format!("{} is an empty archive - nothing was installed", archive.display()))?;
    validate_language(&language)?;
    Ok(language)
}

/// The first component of an archive entry's path; refuses absolute paths
/// and `..`.
fn archive_top_level(path: &Path, archive: &Path) -> Result<String> {
    let mut first = None;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                if first.is_none() {
                    first = Some(name.to_string_lossy().into_owned());
                }
            }
            _ => bail!(
                "{} contains {}, a path outside the archive - nothing was installed",
                archive.display(),
                path.display()
            ),
        }
    }
    first.with_context(|| format!("{} contains an entry with an empty path", archive.display()))
}

fn for_each_entry(
    archive: &Path,
    mut visit: impl FnMut(&Path, &tar::Entry<'_, flate2::read::GzDecoder<File>>) -> Result<()>,
) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("cannot open {}", archive.display()))?;
    let mut reader = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries =
        reader.entries().with_context(|| format!("{} is not a .tar.gz archive", archive.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("{} is not a readable .tar.gz archive", archive.display()))?;
        let path =
            entry.path().with_context(|| format!("{} has an unreadable entry name", archive.display()))?;
        visit(&path, &entry)?;
    }
    Ok(())
}

/// Unpacks `archive` into the staging directory after checking its layout,
/// and returns the staged `<language>/` directory.
fn unpack(archive: &Path, expected_language: Option<&str>, staging: &Staging) -> Result<PathBuf> {
    let language = archive_language(archive)?;
    if let Some(expected) = expected_language {
        if language != expected {
            bail!(
                "{} holds a {language}/ directory, not {expected}/ - nothing was installed",
                archive.display()
            );
        }
    }

    let unpack_dir = staging.path().join("unpack");
    fs::create_dir_all(&unpack_dir).with_context(|| format!("failed to create {}", unpack_dir.display()))?;
    let file = File::open(archive).with_context(|| format!("cannot open {}", archive.display()))?;
    let mut reader = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in reader.entries().with_context(|| format!("cannot read {}", archive.display()))? {
        let mut entry = entry.with_context(|| format!("cannot read {}", archive.display()))?;
        entry.unpack_in(&unpack_dir).with_context(|| format!("failed to unpack {}", archive.display()))?;
    }

    let staged = unpack_dir.join(&language);
    validate_plugin_dir(&staged, &language, &archive.display().to_string())?;
    Ok(staged)
}

/// Checks a staged plugin directory is one discovery would load: a
/// `plugin.toml` that parses, declares `language`, and spawns a binary that
/// is present when it names one inside the plugin.
fn validate_plugin_dir(dir: &Path, language: &str, source: &str) -> Result<()> {
    if !dir.join(MANIFEST_FILE_NAME).is_file() {
        bail!("{source} has no {language}/{MANIFEST_FILE_NAME} - nothing was installed");
    }
    let manifest = manifest::read_manifest(dir).with_context(|| {
        format!("{source} does not carry a valid {language} plugin - nothing was installed")
    })?;
    if manifest.command.starts_with(dir) && !manifest.command.is_file() {
        bail!(
            "{source} has no {}, the binary its {MANIFEST_FILE_NAME} spawns - nothing was installed",
            manifest.command.strip_prefix(dir).unwrap_or(&manifest.command).display()
        );
    }
    Ok(())
}

/// Moves the staged plugin to `<root>/<language>`, replacing an installed
/// one only once the new one is in place.
fn place(staged: &Path, root: &Path, staging: &Staging, out: &mut impl Write) -> Result<()> {
    let language = staged.file_name().context("staged plugin has no directory name")?;
    let dest = root.join(language);
    let new_version = manifest::read_manifest(staged)?.plugin_version;

    let previous = match fs::symlink_metadata(&dest) {
        Ok(_) => {
            let old_version = manifest::read_manifest(&dest).ok().map(|m| m.plugin_version);
            let aside = staging.path().join("previous");
            fs::rename(&dest, &aside)
                .with_context(|| format!("failed to move the installed {} aside", dest.display()))?;
            Some((aside, old_version))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dest.display())),
    };

    if let Err(err) = fs::rename(staged, &dest) {
        if let Some((aside, _)) = &previous {
            let _ = fs::rename(aside, &dest);
        }
        return Err(err).with_context(|| format!("failed to move the plugin into {}", dest.display()));
    }

    let language = language.to_string_lossy();
    match previous {
        Some((_, Some(old_version))) => writeln!(
            out,
            "installed the {language} plugin {new_version} into {} (replacing {old_version})",
            dest.display()
        )?,
        Some((_, None)) => writeln!(
            out,
            "installed the {language} plugin {new_version} into {} (replacing what was there)",
            dest.display()
        )?,
        None => writeln!(out, "installed the {language} plugin {new_version} into {}", dest.display())?,
    }
    warn_if_shadowed(&language, root, out)?;
    writeln!(out, "{RESTART_HINT}")?;
    Ok(())
}

/// Discovery takes a language from the first root that has it, so a copy in
/// an earlier root (`~/.g-mesh/plugins/`) wins over the one just installed.
fn warn_if_shadowed(language: &str, root: &Path, out: &mut impl Write) -> Result<()> {
    for earlier in manifest::default_roots().into_iter().take_while(|candidate| candidate != root) {
        let other = earlier.join(language);
        if other.join(MANIFEST_FILE_NAME).is_file() {
            writeln!(
                out,
                "note: {} is found first, so the daemon will keep using that copy until it is removed",
                other.display()
            )?;
        }
    }
    Ok(())
}

/// Recursively copies a plugin directory. Symlinks are refused rather than
/// followed, so the copy is exactly the files under `from`.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))? {
        let entry = entry.with_context(|| format!("cannot read {}", from.display()))?;
        let source = entry.path();
        let dest = to.join(entry.file_name());
        let kind = entry.file_type().with_context(|| format!("cannot read {}", source.display()))?;
        if kind.is_dir() {
            copy_tree(&source, &dest)?;
        } else if kind.is_file() {
            fs::copy(&source, &dest)
                .with_context(|| format!("failed to copy {} to {}", source.display(), dest.display()))?;
        } else {
            bail!("{} is a symlink or special file; nothing was installed", source.display());
        }
    }
    Ok(())
}

/// A scratch directory inside the target root (so the final rename never
/// crosses a filesystem), removed with everything in it when dropped. Its
/// name starts with a dot and holds no `plugin.toml` at its top level, so
/// discovery never takes it for a plugin.
struct Staging(PathBuf);

impl Staging {
    fn create(root: &Path, language: &str) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("failed to create {}", root.display()))?;
        let path = root.join(format!(".install-{language}-{}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).with_context(|| format!("failed to clear {}", path.display()))?;
        }
        fs::create_dir(&path).with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
