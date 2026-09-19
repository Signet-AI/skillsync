use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

const FORMAT: &str = "skillsync-baseline-transfer";
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
    relationship: String,
    source: String,
    source_path: String,
    skill: String,
    branch: String,
    branch_policy: Option<crate::BranchPolicy>,
    resolved_commit: Option<String>,
    resolved_tree: Option<String>,
    baseline_hash: String,
    tree_hash: String,
    entries: Vec<Entry>,
}
fn hex(b: impl AsRef<[u8]>) -> String {
    b.as_ref().iter().map(|x| format!("{x:02x}")).collect()
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn rel(s: &str) -> Result<()> {
    if s.is_empty()
        || s.contains('\\')
        || Path::new(s).is_absolute()
        || s.chars().any(|c| c.is_control())
        || Path::new(s)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        Err(anyhow!("invalid portable path"))
    } else {
        Ok(())
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
fn scan(root: &Path) -> Result<Vec<Entry>> {
    if !root.is_dir() || fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(anyhow!("baseline package must be a directory"));
    }
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(root).follow_links(false) {
        let e = e?;
        if e.path() == root {
            continue;
        }
        let m = fs::symlink_metadata(e.path())?;
        crate::filesystem::reject_reparse_point(e.path(), "baseline entry")?;
        if m.file_type().is_symlink() || (!m.is_file() && !m.is_dir()) {
            return Err(anyhow!("baseline contains unsafe entry"));
        }
        if m.is_file() {
            let p = e
                .path()
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            rel(&p)?;
            #[cfg(unix)]
            use std::os::unix::fs::PermissionsExt;
            let mode = {
                #[cfg(unix)]
                {
                    m.permissions().mode() & 0o777
                }
                #[cfg(not(unix))]
                {
                    0o644
                }
            };
            if !matches!(mode, 0o644 | 0o755) {
                return Err(anyhow!("unsafe file mode"));
            }
            out.push(Entry {
                path: p,
                sha256: hex(Sha256::digest(&fs::read(e.path())?)),
                mode,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
fn validate_manifest(root: &Path) -> Result<(Manifest, Vec<Entry>)> {
    let m: Manifest = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)
        .context("invalid baseline transfer manifest")?;
    if m.format != FORMAT || m.version != VERSION {
        return Err(anyhow!("unsupported baseline transfer format or version"));
    }
    crate::repository::validate_portable_source_identity(&m.source)?;
    rel(&m.source_path)?;
    crate::strict_component(&m.skill, "skill")?;
    crate::repository::validate_branch(&m.branch)?;
    if m.relationship != crate::relationship_key(&m.source, &m.source_path) {
        return Err(anyhow!("relationship identity mismatch"));
    }
    if !valid_hash(&m.baseline_hash) || !valid_hash(&m.tree_hash) {
        return Err(anyhow!("invalid hash"));
    }
    if crate::filesystem::hash_dir(&root.join("package"))? != m.baseline_hash {
        return Err(anyhow!("baseline package content hash mismatch"));
    }
    for x in [&m.resolved_commit, &m.resolved_tree].into_iter().flatten() {
        if x.len() != 40
            || !x
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(anyhow!("invalid provenance"));
        }
    }
    let actual = scan(&root.join("package"))?;
    let mut seen = BTreeSet::new();
    for e in &m.entries {
        rel(&e.path)?;
        if !seen.insert(&e.path) || !valid_hash(&e.sha256) || !matches!(e.mode, 0o644 | 0o755) {
            return Err(anyhow!("invalid entry"));
        }
    }
    if actual != m.entries || tree_hash(&actual) != m.tree_hash {
        return Err(anyhow!("forged baseline tree"));
    }
    if crate::filesystem::manifest_name(&root.join("package"))? != m.skill {
        return Err(anyhow!("SKILL.md identity mismatch"));
    }
    Ok((m, actual))
}
pub(crate) fn inspect(from: &Path) -> Result<serde_json::Value> {
    let (m, e) = validate_manifest(from)?;
    Ok(
        serde_json::json!({"format":m.format,"version":m.version,"relationship":m.relationship,"source":m.source,"source_path":m.source_path,"skill":m.skill,"branch":m.branch,"branch_policy":m.branch_policy,"resolved_commit":m.resolved_commit,"resolved_tree":m.resolved_tree,"baseline_hash":m.baseline_hash,"tree_hash":m.tree_hash,"entry_count":e.len()}),
    )
}
pub(crate) fn install(a: &crate::App, from: &Path, yes: bool) -> Result<serde_json::Value> {
    if !yes {
        return Err(anyhow!("approval required: pass --yes"));
    }
    if !a.state_path.is_file() {
        return Err(anyhow!(
            "target is not initialized; run init before installing"
        ));
    }
    let (manifest, entries) = validate_manifest(from)?;
    let destination = a.library.join(&manifest.skill);
    let library_identity = crate::filesystem::canonicalize_path_with_missing(&a.library)?;
    let destination_identity = crate::filesystem::canonicalize_path_with_missing(&destination)?;
    if !destination_identity.starts_with(&library_identity)
        || destination_identity == library_identity
    {
        return Err(anyhow!("invalid baseline package destination"));
    }
    if destination.exists() {
        let metadata = fs::symlink_metadata(&destination)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(anyhow!("unsafe existing package destination"));
        }
        if scan(&destination)? == entries {
            return Ok(serde_json::json!({"package":manifest.skill,"status":"already_present"}));
        }
        return Err(anyhow!(
            "package destination exists with different contents; not overwritten"
        ));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    if parent != a.library {
        return Err(anyhow!("baseline package destination escaped library"));
    }
    let temp = tempfile::Builder::new()
        .prefix(".skillsync-baseline-install-")
        .tempdir_in(&a.library)?;
    let staged = temp.path().join("package");
    fs::create_dir(&staged)?;
    crate::filesystem::copy_tree(&from.join("package"), &staged)?;
    if scan(&staged)? != entries {
        return Err(anyhow!("staged baseline package verification failed"));
    }
    crate::filesystem::install_dir_noreplace(&staged, &destination)
        .context("install canonical baseline package without replacement")?;
    if scan(&destination)? != entries {
        return Err(anyhow!("installed baseline package verification failed"));
    }
    Ok(
        serde_json::json!({"package":manifest.skill,"status":"installed","entry_count":entries.len()}),
    )
}

pub(crate) fn export(a: &crate::App, relationship: &str, out: &Path) -> Result<serde_json::Value> {
    let s = a
        .state
        .subscriptions
        .get(relationship)
        .ok_or_else(|| anyhow!("subscription relationship not found"))?;
    if s.status == "conflict" {
        return Err(anyhow!(
            "conflict status requires conflict workspace export"
        ));
    }
    if !matches!(s.status.as_str(), "synced" | "customized") {
        return Err(anyhow!(
            "baseline export requires synced or customized status"
        ));
    }
    crate::repository::validate_portable_source_identity(&s.source)?;
    if relationship != crate::relationship_key(&s.source, &s.source_path) {
        return Err(anyhow!("relationship identity mismatch"));
    }
    crate::strict_component(&s.skill, "skill")?;
    crate::repository::validate_branch(&s.branch)?;
    let baseline = PathBuf::from(&s.baseline_path);
    if baseline != a.baselines.join(relationship)
        || crate::filesystem::hash_dir(&baseline)? != s.baseline_hash
        || crate::filesystem::manifest_name(&baseline)? != s.skill
    {
        return Err(anyhow!("baseline validation failed"));
    }
    if out.exists() {
        return Err(anyhow!(
            "destination already exists; no replacement performed"
        ));
    }
    let source_package = PathBuf::from(&s.source).join(&s.source_path);
    crate::filesystem::reject_output_overlap(
        out,
        &[
            &a.config,
            &a.state_path,
            &a.baselines,
            &a.recovery,
            &a.library,
            &source_package,
        ],
    )?;
    let parent = out
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    if !parent.is_dir() {
        return Err(anyhow!("destination parent missing"));
    }
    let entries = scan(&baseline)?;
    let m = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        relationship: relationship.into(),
        source: s.source.clone(),
        source_path: s.source_path.clone(),
        skill: s.skill.clone(),
        branch: s.branch.clone(),
        branch_policy: s.branch_policy.clone(),
        resolved_commit: s.resolved_commit.clone(),
        resolved_tree: s.resolved_tree.clone(),
        baseline_hash: s.baseline_hash.clone(),
        tree_hash: tree_hash(&entries),
        entries: entries.clone(),
    };
    let tmp = tempfile::tempdir_in(parent)?;
    let staged = tmp.path().join("artifact");
    fs::create_dir_all(staged.join("package"))?;
    crate::filesystem::copy_tree(&baseline, &staged.join("package"))?;
    fs::write(staged.join("manifest.json"), serde_json::to_vec_pretty(&m)?)?;
    validate_manifest(&staged)?;
    crate::filesystem::install_dir_noreplace(&staged, out)?;
    validate_manifest(out)?;
    Ok(
        serde_json::json!({"format":FORMAT,"version":VERSION,"status":"exported","out":out,"relationship":relationship,"entry_count":entries.len()}),
    )
}
