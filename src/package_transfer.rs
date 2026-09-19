use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

const FORMAT: &str = "skillsync-package-transfer";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    sha256: String,
    mode: u32,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    version: u32,
    package: String,
    tree_hash: String,
    entries: Vec<Entry>,
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
fn portable_rel(s: &str, label: &str) -> Result<()> {
    if s.is_empty()
        || s.contains('\\')
        || Path::new(s).is_absolute()
        || s.chars().any(|c| c.is_control())
    {
        return Err(anyhow!("invalid {label}"));
    }
    let p = Path::new(s);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(anyhow!("invalid {label}"));
    }
    Ok(())
}
fn package_identity(s: &str) -> Result<()> {
    portable_rel(s, "package identity")
}
fn mode(path: &Path) -> Result<u32> {
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() || !m.is_file() {
        return Err(anyhow!(
            "package contains unsupported entry: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = m.permissions().mode() & 0o777;
        if !matches!(p, 0o644 | 0o755) {
            return Err(anyhow!("unsafe file mode"));
        }
        Ok(p)
    }
    #[cfg(not(unix))]
    {
        Ok(0o644)
    }
}
fn tree_hash(entries: &[Entry]) -> String {
    let mut h = Sha256::new();
    for e in entries {
        h.update(e.path.as_bytes());
        h.update([0]);
        h.update(e.sha256.as_bytes());
        h.update([0]);
        h.update(e.mode.to_string().as_bytes());
        h.update([10]);
    }
    hex(h.finalize())
}
fn scan(package: &Path) -> Result<Vec<Entry>> {
    if !package.is_dir() || fs::symlink_metadata(package)?.file_type().is_symlink() {
        return Err(anyhow!("package tree must be a regular directory"));
    }
    crate::filesystem::reject_reparse_point(package, "package root")?;
    let mut entries = Vec::new();
    for item in walkdir::WalkDir::new(package).follow_links(false) {
        let item = item?;
        let p = item.path();
        if p == package {
            continue;
        }
        let rel = p
            .strip_prefix(package)?
            .to_string_lossy()
            .replace('\\', "/");
        portable_rel(&rel, "entry path")?;
        let meta = fs::symlink_metadata(p)?;
        crate::filesystem::reject_reparse_point(p, "package entry")?;
        if meta.file_type().is_symlink() || (!meta.is_file() && !meta.is_dir()) {
            return Err(anyhow!("symlink, reparse point, or special file rejected"));
        }
        if meta.is_file() {
            let bytes = fs::read(p)?;
            entries.push(Entry {
                path: rel,
                sha256: hex(Sha256::digest(&bytes)),
                mode: mode(p)?,
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}
fn read_manifest(source: &Path) -> Result<(Manifest, Vec<Entry>, PathBuf)> {
    no_follow_identity(source)?;
    if !source.is_dir() || fs::symlink_metadata(source)?.file_type().is_symlink() {
        return Err(anyhow!(
            "package transfer source must be a regular directory"
        ));
    }
    let manifest_path = source.join("manifest.json");
    if !manifest_path.is_file()
        || fs::symlink_metadata(&manifest_path)?
            .file_type()
            .is_symlink()
    {
        return Err(anyhow!("missing manifest.json"));
    }
    let bytes = fs::read(&manifest_path)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("invalid package transfer manifest")?;
    if manifest.format != FORMAT || manifest.version != VERSION {
        return Err(anyhow!("unsupported package transfer format or version"));
    }
    package_identity(&manifest.package)?;
    if !valid_hash(&manifest.tree_hash) {
        return Err(anyhow!("invalid tree hash"));
    }
    let actual = scan(&source.join("package"))?;
    let mut seen = BTreeSet::new();
    for e in &manifest.entries {
        portable_rel(&e.path, "entry path")?;
        if !seen.insert(&e.path) {
            return Err(anyhow!("duplicate package entry"));
        }
        if !valid_hash(&e.sha256) {
            return Err(anyhow!("invalid file hash"));
        }
        if !matches!(e.mode, 0o644 | 0o755) {
            return Err(anyhow!("invalid file mode"));
        }
    }
    if manifest.entries != actual {
        return Err(anyhow!(
            "manifest entries or file hashes do not match package tree"
        ));
    }
    if tree_hash(&actual) != manifest.tree_hash {
        return Err(anyhow!("forged package tree hash"));
    }
    if actual
        .iter()
        .filter(|entry| entry.path == "SKILL.md")
        .count()
        != 1
    {
        return Err(anyhow!("package transfer requires exactly one SKILL.md"));
    }
    let skill_name = crate::filesystem::manifest_name(&source.join("package"))?;
    if skill_name != manifest.package {
        return Err(anyhow!(
            "SKILL.md name does not match transfer package identity"
        ));
    }
    Ok((manifest, actual, source.join("package")))
}
fn no_follow_identity(path: &Path) -> Result<PathBuf> {
    let identity = crate::filesystem::canonicalize_path_with_missing(path)?;
    let mut cursor = Some(identity.as_path());
    while let Some(current) = cursor {
        if fs::symlink_metadata(current).is_ok() {
            crate::filesystem::reject_reparse_point(current, "path component")?;
        }
        cursor = current.parent();
    }
    Ok(identity)
}
fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

pub(crate) fn inspect(source: &Path) -> Result<serde_json::Value> {
    let (m, _, _) = read_manifest(source)?;
    Ok(serde_json::to_value(m)?)
}
pub(crate) fn install(app: &crate::App, source: &Path, yes: bool) -> Result<serde_json::Value> {
    if !yes {
        return Err(anyhow!("approval required: pass --yes"));
    }
    if !app.state_path.is_file() {
        return Err(anyhow!(
            "target is not initialized; run init before installing"
        ));
    }
    let (manifest, entries, package) = read_manifest(source)?;
    if !entries.iter().any(|entry| entry.path == "SKILL.md") {
        return Err(anyhow!("staged package is missing SKILL.md"));
    }
    let destination = app.library.join(&manifest.package);
    let library_identity = no_follow_identity(&app.library)?;
    let destination_identity = no_follow_identity(&destination)?;
    if !destination_identity.starts_with(&library_identity)
        || destination_identity == library_identity
    {
        return Err(anyhow!("invalid package destination"));
    }
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(anyhow!("unsafe existing package destination"));
        }
        let actual = scan(&destination)?;
        if actual == entries {
            return Ok(serde_json::json!({"package":manifest.package,"status":"already_present"}));
        }
        return Err(anyhow!(
            "package destination exists with different contents; not overwritten"
        ));
    }
    let before = tree_hash(&scan(&package)?);
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    if parent != app.library {
        return Err(anyhow!("package destination escaped library"));
    }
    let temp = tempfile::Builder::new()
        .prefix(".skillsync-package-install-")
        .tempdir_in(&app.library)?;
    let staged = temp.path().join("package");
    crate::filesystem::copy_tree(&package, &staged)?;
    if before != tree_hash(&scan(&package)?) || before != tree_hash(&scan(&staged)?) {
        return Err(anyhow!("staged package changed during installation"));
    }
    let (final_manifest, final_entries, _) = read_manifest(source)?;
    if final_manifest != manifest || final_entries != entries {
        return Err(anyhow!(
            "staged package identity changed before installation"
        ));
    }
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_FAIL_PACKAGE_INSTALL_VERIFY").as_deref() == Ok("1") {
        return Err(anyhow!(
            "injected package-install verification failure (test-only)"
        ));
    }
    crate::filesystem::install_dir_noreplace(&staged, &destination)
        .context("install canonical package without replacement")?;
    let installed = scan(&destination)?;
    if installed != entries {
        return Err(anyhow!(
            "installed package verification failed; existing content retained"
        ));
    }
    Ok(
        serde_json::json!({"package":manifest.package,"status":"installed","entry_count":entries.len()}),
    )
}
pub(crate) fn stage(source: &Path, out: &Path) -> Result<serde_json::Value> {
    let source_identity = no_follow_identity(source)?;
    let output_identity = no_follow_identity(out)?;
    if paths_overlap(&source_identity, &output_identity) {
        return Err(anyhow!(
            "stage destination overlaps package transfer source"
        ));
    }
    if out.exists() {
        return Err(anyhow!(
            "stage destination already exists; no replacement performed"
        ));
    }
    let config = crate::config_dir();
    let config_file = config.join("config.toml");
    #[derive(Deserialize, Default)]
    struct TransferConfig {
        library: Option<String>,
    }
    let configured = if fs::symlink_metadata(&config_file).is_ok() {
        toml::from_str::<TransferConfig>(&fs::read_to_string(&config_file)?)?
            .library
            .map(PathBuf::from)
    } else {
        None
    };
    let library = crate::filesystem::resolve_library_path(
        &crate::filesystem::effective_library_path(configured),
    )?;
    let owned = [
        no_follow_identity(&config)?,
        no_follow_identity(&config.join("state.json"))?,
        no_follow_identity(&config.join("baselines"))?,
        no_follow_identity(&config.join("recovery"))?,
        no_follow_identity(&library)?,
    ];
    if owned
        .iter()
        .any(|root| paths_overlap(&output_identity, root))
    {
        return Err(anyhow!("stage destination overlaps Skillsync-owned path"));
    }
    let (manifest, entries, package) = read_manifest(source)?;
    let before = tree_hash(&scan(&package)?);
    let parent = out
        .parent()
        .ok_or_else(|| anyhow!("stage destination has no parent"))?;
    fs::create_dir_all(parent)?;
    let temp = tempfile::Builder::new()
        .prefix(".skillsync-package-")
        .tempdir_in(parent)?;
    let staged = temp.path().join("artifact");
    let staged_package = staged.join("package");
    fs::create_dir_all(&staged_package)?;
    crate::filesystem::copy_tree(&package, &staged_package)?;
    if before != tree_hash(&scan(&staged_package)?) || before != tree_hash(&scan(&package)?) {
        return Err(anyhow!("source changed during package staging"));
    }
    fs::write(
        staged.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let (staged_manifest, staged_entries, _) = read_manifest(&staged)?;
    if staged_manifest != manifest || staged_entries != entries {
        return Err(anyhow!(
            "staged package identity does not match source manifest"
        ));
    }
    crate::filesystem::install_dir_noreplace(&staged, out)?;
    Ok(
        serde_json::json!({"format":FORMAT,"version":VERSION,"status":"staged","out":out,"package":manifest.package,"tree_hash":manifest.tree_hash,"entry_count":entries.len(),"non_activating":true,"activation":"not_supported"}),
    )
}
