use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

use crate::filesystem::{
    assert_no_symlink_path, canonicalize_path, canonicalize_path_with_missing, copy_tree, files,
    hash_dir, install_dir_noreplace, manifest_name, replace_dir_bound, safe, snapshot_transaction,
    strict_component, validate_state_path,
};
use crate::{filesystem::StateLock, relationship_key, App};

fn validate_subscription(
    a: &App,
    relationship: &str,
    subscription: &crate::Subscription,
) -> Result<()> {
    if relationship != relationship_key(&subscription.source, &subscription.source_path) {
        return Err(anyhow!(
            "subscription relationship key does not match canonical relationship"
        ));
    }
    strict_component(&subscription.skill, "subscription skill name")?;
    crate::repository::normalize(&subscription.source)?;
    crate::repository::validate_branch(&subscription.branch)?;
    crate::filesystem::source_rel(&subscription.source_path)?;
    if subscription.baseline_source.is_empty() {
        return Err(anyhow!("subscription baseline source is required"));
    }
    if subscription.baseline_source != subscription.source {
        return Err(anyhow!(
            "subscription baseline source does not match source"
        ));
    }
    if subscription.baseline_source_path.is_empty() {
        return Err(anyhow!("subscription baseline source path is required"));
    }
    if subscription.baseline_source_path != subscription.source_path {
        return Err(anyhow!(
            "subscription baseline source path does not match source path"
        ));
    }
    if !matches!(
        subscription.status.as_str(),
        "synced"
            | "customized"
            | "conflict"
            | "changed_during_update"
            | "authentication_required"
            | "offline"
            | "permission_denied"
            | "source_missing"
            | "branch_missing"
            | "package_missing"
            | "invalid_source"
    ) {
        return Err(anyhow!(
            "invalid subscription status: {}",
            subscription.status
        ));
    }
    if let Some(selection) = &subscription.conflict_selection {
        if !matches!(selection.as_str(), "local" | "incoming") {
            return Err(anyhow!("invalid conflict side selection: {selection}"));
        }
    }
    let expected_local = a.library.join(&subscription.skill);
    let local = PathBuf::from(&subscription.local_path);
    if local != expected_local || !local.is_absolute() {
        return Err(anyhow!("subscription local path does not match library"));
    }
    let local_rel = local
        .strip_prefix(&a.library)
        .map_err(|_| anyhow!("subscription destination escaped library"))?;
    if !safe(local_rel) {
        return Err(anyhow!("subscription destination escaped library"));
    }
    assert_no_symlink_path(&a.library, local_rel)?;
    let meta = fs::symlink_metadata(&local)
        .map_err(|_| anyhow!("subscription local package is missing"))?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || canonicalize_path(&local)? != local
        || manifest_name(&local)? != subscription.skill
    {
        return Err(anyhow!(
            "subscription local package is not a canonical matching directory"
        ));
    }
    let baseline = PathBuf::from(&subscription.baseline_path);
    let expected_baseline = a.baselines.join(relationship);
    if baseline != expected_baseline {
        return Err(anyhow!(
            "subscription baseline path does not match state directory"
        ));
    }
    validate_state_path(&a.baselines, &baseline, "baseline")?;
    if let Some(raw) = &subscription.recovery_path {
        let recovery = PathBuf::from(raw);
        validate_state_path(&a.recovery, &recovery, "recovery")?;
        let rel = recovery
            .strip_prefix(&a.recovery)
            .map_err(|_| anyhow!("recovery path escaped configured recovery root"))?;
        if rel.components().count() != 1
            || !rel.components().all(|c| matches!(c, Component::Normal(_)))
            || !rel
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&format!("{relationship}-")))
        {
            return Err(anyhow!(
                "recovery path does not belong to subscription relationship: {relationship}"
            ));
        }
    }
    Ok(())
}

fn validate_baseline_integrity(
    a: &App,
    relationship: &str,
    subscription: &crate::Subscription,
) -> Result<()> {
    let baseline = PathBuf::from(&subscription.baseline_path);
    let expected = a.baselines.join(relationship);
    if baseline != expected {
        return Err(anyhow!(
            "subscription baseline path does not match state directory"
        ));
    }
    validate_state_path(&a.baselines, &baseline, "baseline")?;
    let metadata = fs::symlink_metadata(&baseline)
        .map_err(|_| anyhow!("subscription baseline package is missing"))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || canonicalize_path(&baseline)? != baseline
        || manifest_name(&baseline)? != subscription.skill
    {
        return Err(anyhow!(
            "subscription baseline package is not a canonical matching directory"
        ));
    }
    if hash_dir(&baseline)? != subscription.baseline_hash {
        return Err(anyhow!(
            "subscription baseline content does not match recorded hash"
        ));
    }
    Ok(())
}

/// Inventory only: recovery application remains deliberately unsupported until
/// the persisted schema records an immutable conflict snapshot.
pub(crate) fn list(a: &App) -> Result<serde_json::Value> {
    let mut logical = std::collections::BTreeSet::new();
    for (relationship, subscription) in &a.state.subscriptions {
        validate_subscription(a, relationship, subscription)?;
        validate_baseline_integrity(a, relationship, subscription)?;
        if !logical.insert(relationship_key(
            &subscription.source,
            &subscription.source_path,
        )) {
            return Err(anyhow!("duplicate subscription relationship"));
        }
    }
    let mut conflicts = Vec::new();
    for (relationship, subscription) in &a.state.subscriptions {
        if subscription.status != "conflict" && subscription.recovery_path.is_none() {
            continue;
        }
        if subscription.status != "conflict" {
            continue;
        }
        // Use the same strict manifest/evidence validator as `conflicts show`.
        // Inventory must fail closed rather than emit a weaker representation.
        let validated = show(a, relationship)?;
        conflicts.push(validated);
        continue;
    }
    Ok(json!({"conflicts": conflicts, "count": conflicts.len()}))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConflictManifest {
    manifest_version: u32,
    relationship: String,
    source: String,
    source_path: String,
    path: String,
    base_hash: String,
    base_path: String,
    local_hash: String,
    local_path: String,
    incoming_hash: String,
    incoming_path: String,
    live_hash_at_detection: String,
    baseline_path: String,
    baseline_source: String,
    baseline_source_path: String,
    transition: String,
    status: String,
}

fn validate_evidence_package(
    root: &PathBuf,
    path: &PathBuf,
    skill: &str,
    expected_hash: &str,
    label: &str,
) -> Result<()> {
    validate_state_path(root, path, label)?;
    let meta =
        fs::symlink_metadata(path).map_err(|_| anyhow!("conflict evidence is missing {label}"))?;
    if !meta.is_dir() || meta.file_type().is_symlink() || canonicalize_path(path)? != *path {
        return Err(anyhow!(
            "conflict evidence {label} must be a canonical regular directory"
        ));
    }
    assert_no_symlink_path(root, path.strip_prefix(root)?)?;
    if manifest_name(path)? != skill {
        return Err(anyhow!(
            "conflict evidence {label} manifest does not match subscription"
        ));
    }
    if hash_dir(path)? != expected_hash {
        return Err(anyhow!(
            "conflict evidence {label} hash does not match manifest"
        ));
    }
    Ok(())
}

pub(crate) fn show(a: &App, relationship: &str) -> Result<serde_json::Value> {
    let s = a
        .state
        .subscriptions
        .get(relationship)
        .ok_or_else(|| anyhow!("conflict relationship not found"))?;
    validate_subscription(a, relationship, s)?;
    validate_baseline_integrity(a, relationship, s)?;
    if s.status != "conflict" {
        return Err(anyhow!("relationship is not an open conflict"));
    }
    let recovery = PathBuf::from(
        s.recovery_path
            .as_ref()
            .ok_or_else(|| anyhow!("conflict has no recovery evidence"))?,
    );
    validate_state_path(&a.recovery, &recovery, "recovery")?;
    let meta = fs::symlink_metadata(&recovery)?;
    if !meta.is_dir() || meta.file_type().is_symlink() || canonicalize_path(&recovery)? != recovery
    {
        return Err(anyhow!("conflict recovery is not a canonical directory"));
    }
    let raw = fs::read(recovery.join("manifest.json"))
        .map_err(|_| anyhow!("conflict manifest is missing"))?;
    let manifest: ConflictManifest =
        serde_json::from_slice(&raw).map_err(|_| anyhow!("conflict manifest is malformed"))?;
    if manifest.manifest_version != 1
        || manifest.relationship != relationship
        || manifest.source != s.source
        || manifest.source_path != s.source_path
        || manifest.path != s.local_path
        || manifest.baseline_path != s.baseline_path
        || manifest.baseline_source != s.source
        || manifest.baseline_source_path != s.source_path
        || manifest.status != "open"
        || manifest.transition != "directory"
    {
        return Err(anyhow!("conflict manifest is incompatible or mismatched"));
    }
    crate::repository::normalize(&manifest.source)?;
    crate::repository::validate_branch(&s.branch)?;
    crate::filesystem::source_rel(&manifest.source_path)?;
    if manifest.base_hash != s.baseline_hash {
        return Err(anyhow!(
            "conflict base evidence does not match subscription baseline"
        ));
    }
    // Verify the persisted baseline itself, not only the copied evidence.
    validate_baseline_integrity(a, relationship, s)?;
    for (value, label) in [
        (&manifest.base_hash, "base"),
        (&manifest.local_hash, "local"),
        (&manifest.incoming_hash, "incoming"),
        (&manifest.live_hash_at_detection, "live"),
    ] {
        if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(anyhow!("invalid {label} evidence hash"));
        }
    }
    let paths = [
        (
            "base",
            manifest.base_path.clone(),
            manifest.base_hash.clone(),
        ),
        (
            "local",
            manifest.local_path.clone(),
            manifest.local_hash.clone(),
        ),
        (
            "incoming",
            manifest.incoming_path.clone(),
            manifest.incoming_hash.clone(),
        ),
    ];
    for (label, raw_path, hash) in paths {
        let path = PathBuf::from(raw_path);
        if path != recovery.join(label) {
            return Err(anyhow!(
                "conflict {label} path does not match contained evidence"
            ));
        }
        validate_evidence_package(&a.recovery, &path, &s.skill, &hash, label)?;
    }
    if manifest.live_hash_at_detection != manifest.local_hash {
        return Err(anyhow!("conflict live hash does not match local evidence"));
    }
    let current_live_hash = hash_dir(&PathBuf::from(&s.local_path))?;
    let mut output = serde_json::to_value(manifest)?;
    output["status"] = json!(s.status);
    output["current_live_hash"] = json!(current_live_hash.clone());
    output["stale"] = json!(current_live_hash != output["live_hash_at_detection"]);
    Ok(output)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportManifest {
    manifest_version: u32,
    workspace_kind: String,
    status: String,
    relationship: String,
    source: String,
    source_path: String,
    skill: String,
    base_hash: String,
    local_hash: String,
    incoming_hash: String,
    live_hash_at_export: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedManifest {
    manifest_version: u32,
    workspace_kind: String,
    status: String,
    relationship: String,
    source: String,
    source_path: String,
    skill: String,
    base_hash: String,
    local_hash: String,
    incoming_hash: String,
    live_hash_at_export: String,
    resolved_tree: String,
    resolved_hash: String,
    unresolved_markers: bool,
}

fn validate_resolved_workspace(
    workspace: &Path,
    relationship: &str,
    s: &crate::Subscription,
    evidence: &serde_json::Value,
) -> Result<PathBuf> {
    validate_workspace_spelling(workspace)?;
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace)
    };
    let workspace = canonicalize_path(&absolute)?;
    validate_workspace_tree(&workspace, "workspace")?;
    let entries = fs::read_dir(&workspace)?
        .map(|e| e.map(|x| x.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    let allowed = ["manifest.json", "base", "local", "incoming", "resolved"]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if entries != allowed {
        return Err(anyhow!("workspace contains unexpected entries"));
    }
    let m: ResolvedManifest = serde_json::from_slice(&fs::read(workspace.join("manifest.json"))?)
        .map_err(|_| anyhow!("resolved workspace manifest is malformed"))?;
    if m.manifest_version != 2
        || m.workspace_kind != "conflict-resolution"
        || m.status != "resolved"
        || m.relationship != relationship
        || m.source != s.source
        || m.source_path != s.source_path
        || m.skill != s.skill
        || m.resolved_tree != "resolved"
        || m.unresolved_markers
    {
        return Err(anyhow!("resolved workspace identity or status mismatch"));
    }
    for (field, actual) in [
        ("base_hash", &m.base_hash),
        ("local_hash", &m.local_hash),
        ("incoming_hash", &m.incoming_hash),
    ] {
        if evidence[field].as_str() != Some(actual.as_str()) {
            return Err(anyhow!(
                "resolved workspace {field} does not match conflict evidence"
            ));
        }
    }
    if m.base_hash != s.baseline_hash {
        return Err(anyhow!(
            "resolved workspace base does not match subscription baseline"
        ));
    }
    if m.live_hash_at_export != m.local_hash {
        return Err(anyhow!(
            "resolved workspace live hash does not match local evidence"
        ));
    }
    for (v, label) in [
        (&m.base_hash, "base"),
        (&m.local_hash, "local"),
        (&m.incoming_hash, "incoming"),
        (&m.live_hash_at_export, "live"),
        (&m.resolved_hash, "resolved"),
    ] {
        if v.len() != 64
            || !v
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(anyhow!("invalid {label} workspace hash"));
        }
    }
    for side in ["base", "local", "incoming"] {
        validate_workspace_tree(&workspace.join(side), side)?;
        if manifest_name(&workspace.join(side))? != s.skill {
            return Err(anyhow!("workspace {side} identity mismatch"));
        }
        let expected = &m.base_hash;
        let expected = match side {
            "local" => &m.local_hash,
            "incoming" => &m.incoming_hash,
            _ => expected,
        };
        if hash_dir(&workspace.join(side))? != *expected {
            return Err(anyhow!(
                "workspace {side} hash does not match conflict evidence"
            ));
        }
    }
    let resolved = workspace.join("resolved");
    validate_workspace_tree(&resolved, "resolved")?;
    if manifest_name(&resolved)? != s.skill || hash_dir(&resolved)? != m.resolved_hash {
        return Err(anyhow!("resolved workspace hash or identity mismatch"));
    }
    for file in files(&resolved)? {
        if file.0.file_name().is_some_and(|n| n == "SKILL.md") {
            let text = String::from_utf8_lossy(&file.1);
            if text.lines().any(|line| {
                ["<<<<<<<", "=======", ">>>>>>>"]
                    .iter()
                    .any(|x| line.starts_with(x))
            }) {
                return Err(anyhow!(
                    "resolved workspace contains unresolved conflict markers"
                ));
            }
        }
    }
    Ok(resolved)
}

fn validate_export_destination(out: &std::path::Path) -> Result<(PathBuf, PathBuf)> {
    let destination = canonicalize_path_with_missing(out)?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("workspace destination has no parent"))?
        .to_path_buf();
    let mut cursor = parent.clone();
    loop {
        let metadata = fs::symlink_metadata(&cursor)
            .map_err(|_| anyhow!("workspace ancestor is missing: {}", cursor.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || canonicalize_path(&cursor)? != cursor
        {
            return Err(anyhow!("workspace destination has an unsafe ancestor"));
        }
        if cursor.parent().is_none() || cursor.as_os_str() == "/" {
            break;
        }
        cursor.pop();
    }
    if let Ok(metadata) = fs::symlink_metadata(&destination) {
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || canonicalize_path(&destination)? != destination
        {
            return Err(anyhow!(
                "workspace destination is not a regular absent directory"
            ));
        }
        return Err(anyhow!("workspace destination already exists"));
    }
    Ok((destination, parent))
}

pub(crate) fn export(
    a: &App,
    relationship: &str,
    out: &std::path::Path,
) -> Result<serde_json::Value> {
    let (destination, parent) = validate_export_destination(out)?;
    let view = show(a, relationship)?;
    let subscription = a.state.subscriptions.get(relationship).unwrap();
    if std::path::Path::new(&subscription.source).is_absolute() {
        return Err(anyhow!(
            "non-portable local repository source; conflict export refused"
        ));
    }
    let live_hash = hash_dir(std::path::Path::new(&subscription.local_path))?;
    let detected = view["live_hash_at_detection"].as_str().unwrap();
    if live_hash != detected {
        return Err(anyhow!(
            "live content changed since conflict detection; export refused"
        ));
    }
    let recovery = PathBuf::from(subscription.recovery_path.as_ref().unwrap());
    let staged_parent = tempfile::tempdir_in(&parent)?;
    let result = (|| {
        let staged = staged_parent.path().join("workspace");
        fs::create_dir(&staged)?;
        for side in ["base", "local", "incoming"] {
            let source = recovery.join(side);
            validate_evidence_package(
                &a.recovery,
                &source,
                &subscription.skill,
                view[&format!("{side}_hash")].as_str().unwrap(),
                side,
            )?;
            copy_tree(&source, &staged.join(side))?;
            if hash_dir(&staged.join(side))? != view[&format!("{side}_hash")].as_str().unwrap() {
                return Err(anyhow!("copied {side} evidence hash mismatch"));
            }
        }
        let manifest = ExportManifest {
            manifest_version: 1,
            workspace_kind: "conflict-resolution".into(),
            status: "immutable".into(),
            relationship: relationship.into(),
            source: subscription.source.clone(),
            source_path: subscription.source_path.clone(),
            skill: subscription.skill.clone(),
            base_hash: view["base_hash"].as_str().unwrap().into(),
            local_hash: view["local_hash"].as_str().unwrap().into(),
            incoming_hash: view["incoming_hash"].as_str().unwrap().into(),
            live_hash_at_export: live_hash,
        };
        let raw = serde_json::to_vec_pretty(&manifest)?;
        let _: ExportManifest = serde_json::from_slice(&raw)?;
        fs::write(staged.join("manifest.json"), raw)?;
        install_dir_noreplace(&staged, &destination)
            .map_err(|e| anyhow!("workspace publication refused: {e}"))?;
        Ok(json!({"relationship": relationship, "status": "exported", "workspace": destination}))
    })();
    let cleanup = staged_parent.close();
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(anyhow!(
            "export published but staging cleanup failed; recovery required: {error}"
        )),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(anyhow!(
            "export failed: {error}; staging cleanup also failed; recovery required: {cleanup_error}"
        )),
    }
}

fn validate_workspace_spelling(workspace: &Path) -> Result<()> {
    let supplied = workspace.to_string_lossy();
    let raw = supplied.as_ref();
    if raw.contains('\\')
        || raw.starts_with("//")
        || raw.as_bytes().get(1).is_some_and(|b| *b == b':')
    {
        return Err(anyhow!("workspace path has an unsupported spelling"));
    }
    for (index, segment) in raw.split('/').enumerate() {
        if segment == "." || segment == ".." {
            return Err(anyhow!("workspace path contains traversal or dot segments"));
        }
        if segment.is_empty() && index != 0 {
            return Err(anyhow!("workspace path has unsafe separators"));
        }
    }
    Ok(())
}

fn validate_workspace_tree(root: &Path, label: &str) -> Result<()> {
    fn walk(root: &Path, current: &Path, rel: &Path, label: &str) -> Result<()> {
        let mut entries = fs::read_dir(current)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let child_rel = rel.join(entry.file_name());
            if !safe(&child_rel) {
                return Err(anyhow!("workspace {label} contains an unsafe path"));
            }
            crate::filesystem::reject_reparse_point(&path, "workspace descendant")?;
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!("workspace {label} contains a symlink"));
            }
            if metadata.is_dir() {
                if canonicalize_path(&path)? != path {
                    return Err(anyhow!("workspace {label} contains an unsafe directory"));
                }
                walk(root, &path, &child_rel, label)?;
            } else if metadata.is_file() {
                if canonicalize_path(&path)? != path {
                    return Err(anyhow!("workspace {label} contains an unsafe file"));
                }
            } else {
                return Err(anyhow!("workspace {label} contains an unsupported entry"));
            }
        }
        let _ = root;
        Ok(())
    }
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || canonicalize_path(root)? != root {
        return Err(anyhow!("workspace {label} is not a canonical directory"));
    }
    walk(root, root, Path::new(""), label)
}

pub(crate) fn inspect_workspace(workspace: &Path) -> Result<serde_json::Value> {
    validate_workspace_spelling(workspace)?;
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace)
    };
    let mut cursor = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => cursor.push(prefix.as_os_str()),
            Component::RootDir => cursor.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::Normal(name) => {
                cursor.push(name);
                let metadata = fs::symlink_metadata(&cursor).map_err(|_| {
                    anyhow!("workspace path component is missing: {}", cursor.display())
                })?;
                crate::filesystem::reject_reparse_point(&cursor, "workspace ancestor")?;
                if metadata.file_type().is_symlink() {
                    return Err(anyhow!(
                        "workspace path component is a symlink: {}",
                        cursor.display()
                    ));
                }
                if !metadata.is_dir() {
                    return Err(anyhow!(
                        "workspace path component is not a directory: {}",
                        cursor.display()
                    ));
                }
            }
            Component::CurDir | Component::ParentDir => {
                return Err(anyhow!("workspace path contains traversal or dot segments"));
            }
        }
    }
    let workspace = canonicalize_path(&absolute)?;
    let meta = fs::symlink_metadata(&workspace)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || canonicalize_path(&workspace)? != workspace
    {
        return Err(anyhow!("workspace must be a canonical regular directory"));
    }
    let mut entries = BTreeSet::new();
    for entry in fs::read_dir(&workspace)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entries.insert(name) {
            return Err(anyhow!("duplicate workspace entry"));
        }
        let m = fs::symlink_metadata(entry.path())?;
        if m.file_type().is_symlink() || (!m.is_dir() && !m.is_file()) {
            return Err(anyhow!("workspace contains unsafe entry"));
        }
    }
    let allowed = ["base", "incoming", "local", "manifest.json"]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if entries != allowed {
        return Err(anyhow!("workspace contains unexpected entries"));
    }
    let manifest: ExportManifest =
        serde_json::from_slice(&fs::read(workspace.join("manifest.json"))?)
            .map_err(|_| anyhow!("workspace manifest is malformed"))?;
    if manifest.manifest_version != 1
        || manifest.workspace_kind != "conflict-resolution"
        || manifest.status != "immutable"
    {
        return Err(anyhow!("workspace manifest is incompatible"));
    }
    strict_component(&manifest.skill, "workspace skill")?;
    if manifest.relationship != relationship_key(&manifest.source, &manifest.source_path) {
        return Err(anyhow!("workspace relationship identity mismatch"));
    }
    crate::repository::normalize(&manifest.source)?;
    crate::filesystem::source_rel(&manifest.source_path)?;
    for (value, label) in [
        (&manifest.base_hash, "base"),
        (&manifest.local_hash, "local"),
        (&manifest.incoming_hash, "incoming"),
        (&manifest.live_hash_at_export, "live"),
    ] {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(anyhow!("invalid {label} evidence hash"));
        }
    }
    for (side, expected) in [
        ("base", &manifest.base_hash),
        ("local", &manifest.local_hash),
        ("incoming", &manifest.incoming_hash),
    ] {
        let path = workspace.join(side);
        validate_workspace_tree(&path, side)?;
        let m = fs::symlink_metadata(&path)?;
        if !m.is_dir()
            || m.file_type().is_symlink()
            || canonicalize_path(&path)? != path
            || manifest_name(&path)? != manifest.skill
            || hash_dir(&path)? != *expected
        {
            return Err(anyhow!("workspace {side} evidence is invalid"));
        }
    }
    Ok(
        json!({"status":"valid_immutable","identity":{"relationship":manifest.relationship,"source":manifest.source,"source_path":manifest.source_path,"skill":manifest.skill},"side_hashes":{"base":manifest.base_hash,"local":manifest.local_hash,"incoming":manifest.incoming_hash,"live_hash_at_export":manifest.live_hash_at_export},"selected_tree":null}),
    )
}

pub(crate) fn resolve(
    a: &mut App,
    relationship: &str,
    local: bool,
    incoming: bool,
) -> Result<serde_json::Value> {
    if local == incoming {
        return Err(anyhow!("exactly one conflict side must be selected"));
    }
    let view = show(a, relationship)?;
    let detected = view["live_hash_at_detection"]
        .as_str()
        .ok_or_else(|| anyhow!("conflict manifest has no live detection hash"))?
        .to_owned();
    let local_path = a
        .state
        .subscriptions
        .get(relationship)
        .ok_or_else(|| anyhow!("conflict relationship not found"))?
        .local_path
        .clone();
    if hash_dir(PathBuf::from(&local_path).as_path())? != detected {
        return Err(anyhow!(
            "live content changed since conflict detection; selection refused"
        ));
    }
    let side = if local { "local" } else { "incoming" };
    if a.state.subscriptions[relationship]
        .conflict_selection
        .as_deref()
        == Some(side)
    {
        return Ok(json!({"relationship":relationship,"status":"selected","selected_side":side}));
    }
    a.state
        .subscriptions
        .get_mut(relationship)
        .unwrap()
        .conflict_selection = Some(side.into());
    if let Err(error) = a.save() {
        a.state
            .subscriptions
            .get_mut(relationship)
            .unwrap()
            .conflict_selection = None;
        return Err(error).context("persist conflict side selection");
    }
    Ok(json!({"relationship":relationship,"status":"selected","selected_side":side}))
}

pub(crate) fn resume(
    a: &mut App,
    relationship: &str,
    workspace: Option<&Path>,
    operation_lock: &StateLock,
) -> Result<serde_json::Value> {
    if let Some(workspace) = workspace {
        let s = a
            .state
            .subscriptions
            .get(relationship)
            .cloned()
            .ok_or_else(|| anyhow!("conflict relationship not found"))?;
        let view = show(a, relationship)?;
        let resolved = validate_resolved_workspace(workspace, relationship, &s, &view)?;
        if hash_dir(Path::new(&s.local_path))? != view["live_hash_at_detection"].as_str().unwrap() {
            return Err(anyhow!(
                "live content changed since conflict detection; workspace resume refused"
            ));
        }
        let stage_parent = tempfile::tempdir_in(Path::new(&s.local_path).parent().unwrap())?;
        let staged = stage_parent.path().join("resolved");
        copy_tree(&resolved, &staged)?;
        let parent = crate::filesystem::open_directory_file_bound(
            Path::new(&s.local_path).parent().unwrap(),
        )?;
        let (baseline_path, baseline_hash, mut baseline_replacement) =
            snapshot_transaction(a, relationship, &staged)?;
        let mut live_replacement = replace_dir_bound(Path::new(&s.local_path), &staged, &parent)?;
        let previous = a.state.clone();
        let mut updated = s;
        updated.status = "synced".into();
        updated.conflict_selection = None;
        updated.baseline_path = baseline_path.display().to_string();
        updated.baseline_hash = baseline_hash;
        updated.last_sync = crate::now();
        updated.update_count += 1;
        a.state.subscriptions.insert(relationship.into(), updated);
        live_replacement.prepare()?;
        baseline_replacement.prepare()?;
        if let Err(error) = a.save() {
            a.state = previous;
            return Err(error).context("persist resolved conflict state; replacements rolled back");
        }
        live_replacement.commit()?;
        baseline_replacement.commit()?;
        #[cfg(feature = "test-hooks")]
        if std::env::var("SKILLSYNC_TEST_CORRUPT_RESUME_STATE").as_deref() == Ok("1") {
            std::fs::write(&a.state_path, b"{\"version\":6,\"subscriptions\":null}")?;
        }
        #[cfg(feature = "test-hooks")]
        if std::env::var("SKILLSYNC_TEST_REPLACE_RESUME_CONFIG").as_deref() == Ok("1") {
            let replacement = a.config.with_extension("replaced");
            std::fs::rename(&a.config, &replacement)?;
            std::fs::create_dir_all(&a.config)?;
            std::fs::write(
                a.config.join("state.json"),
                b"{\"version\":6,\"subscriptions\":null}",
            )?;
        }
        #[cfg(feature = "test-hooks")]
        if std::env::var("SKILLSYNC_TEST_FAIL_RESUME_POSTVERIFY").as_deref() == Ok("1") {
            return Err(anyhow!(
                "resume post-commit verification failed; recovery required"
            ));
        }
        let expected = a
            .state
            .subscriptions
            .get(relationship)
            .cloned()
            .ok_or_else(|| anyhow!("resume post-commit verification failed; recovery required"))?;
        let persisted_app = App::load(Some(operation_lock))
            .map_err(|_| anyhow!("resume post-commit verification failed; recovery required"))?;
        let persisted = persisted_app
            .state
            .subscriptions
            .get(relationship)
            .ok_or_else(|| anyhow!("resume post-commit verification failed; recovery required"))?;
        if serde_json::to_value(persisted)? != serde_json::to_value(&expected)? {
            return Err(anyhow!(
                "resume post-commit verification failed; recovery required"
            ));
        }
        validate_subscription(&persisted_app, relationship, persisted)?;
        validate_baseline_integrity(&persisted_app, relationship, persisted)?;
        if hash_dir(Path::new(&persisted.local_path))? != persisted.baseline_hash
            || hash_dir(Path::new(&persisted.baseline_path))? != persisted.baseline_hash
        {
            return Err(anyhow!(
                "resume post-commit verification failed; recovery required"
            ));
        }
        return Ok(
            json!({"relationship":relationship,"status":"synced","workspace_applied":true,"recovery_retained":true}),
        );
    }
    if let Some(existing) = a.state.subscriptions.get(relationship) {
        if existing.status == "synced" && existing.recovery_path.is_some() {
            validate_subscription(a, relationship, existing)?;
            validate_baseline_integrity(a, relationship, existing)?;
            if hash_dir(PathBuf::from(&existing.local_path).as_path())? == existing.baseline_hash {
                return Ok(json!({
                    "relationship": relationship,
                    "status": "synced",
                    "selected_side": existing.conflict_selection,
                    "recovery_retained": true,
                }));
            }
            return Err(anyhow!(
                "relationship was already resumed but live content has newer edits"
            ));
        }
    }
    let view = show(a, relationship)?;
    let s = a
        .state
        .subscriptions
        .get(relationship)
        .cloned()
        .ok_or_else(|| anyhow!("conflict relationship not found"))?;
    let side = s
        .conflict_selection
        .clone()
        .ok_or_else(|| anyhow!("conflict side selection is required; use --local or --incoming"))?;
    let evidence = view[&format!("{side}_path")]
        .as_str()
        .ok_or_else(|| anyhow!("selected conflict evidence path is missing"))?;
    let expected = view[&format!("{side}_hash")]
        .as_str()
        .ok_or_else(|| anyhow!("selected conflict evidence hash is missing"))?;
    let live = PathBuf::from(&s.local_path);
    if hash_dir(&live)? != view["live_hash_at_detection"].as_str().unwrap() {
        return Err(anyhow!(
            "live content changed since conflict detection; resume refused"
        ));
    }
    let stage_parent = tempfile::tempdir_in(
        live.parent()
            .ok_or_else(|| anyhow!("live path has no parent"))?,
    )?;
    let staged = stage_parent.path().join("selected");
    copy_tree(PathBuf::from(evidence).as_path(), &staged)?;
    if hash_dir(&staged)? != expected {
        return Err(anyhow!(
            "selected conflict evidence changed; resume refused"
        ));
    }
    let parent = crate::filesystem::open_directory_file_bound(live.parent().unwrap())?;
    if hash_dir(&live)? != view["live_hash_at_detection"].as_str().unwrap() {
        return Err(anyhow!(
            "live content changed since conflict detection; resume refused"
        ));
    }
    let (baseline_path, baseline_hash, mut baseline_replacement) =
        snapshot_transaction(a, relationship, &staged)?;
    if hash_dir(&live)? != view["live_hash_at_detection"].as_str().unwrap() {
        return Err(anyhow!(
            "live content changed during resume; replacements rolled back"
        ));
    }
    let mut live_replacement = replace_dir_bound(&live, &staged, &parent)?;
    let previous = a.state.clone();
    let mut updated = s;
    updated.status = "synced".into();
    updated.baseline_path = baseline_path.display().to_string();
    updated.baseline_hash = baseline_hash;
    updated.last_sync = crate::now();
    updated.update_count += 1;
    a.state.subscriptions.insert(relationship.into(), updated);
    live_replacement.prepare()?;
    baseline_replacement.prepare()?;
    if let Err(error) = a.save() {
        a.state = previous;
        return Err(error).context("persist resumed conflict state; replacements rolled back");
    }
    live_replacement.commit()?;
    baseline_replacement.commit()?;
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_CORRUPT_RESUME_STATE").as_deref() == Ok("1") {
        std::fs::write(&a.state_path, b"{\"version\":6,\"subscriptions\":null}")?;
    }
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_REPLACE_RESUME_CONFIG").as_deref() == Ok("1") {
        let replacement = a.config.with_extension("replaced");
        std::fs::rename(&a.config, &replacement)?;
        std::fs::create_dir_all(&a.config)?;
        std::fs::write(
            a.config.join("state.json"),
            b"{\"version\":6,\"subscriptions\":null}",
        )?;
    }
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_FAIL_RESUME_POSTVERIFY").as_deref() == Ok("1") {
        return Err(anyhow!(
            "resume post-commit verification failed; recovery required"
        ));
    }
    let expected = a
        .state
        .subscriptions
        .get(relationship)
        .cloned()
        .ok_or_else(|| anyhow!("resume post-commit verification failed; recovery required"))?;
    let persisted_app = App::load(Some(operation_lock))
        .map_err(|_| anyhow!("resume post-commit verification failed; recovery required"))?;
    let persisted = persisted_app
        .state
        .subscriptions
        .get(relationship)
        .ok_or_else(|| anyhow!("resume post-commit verification failed; recovery required"))?;
    if serde_json::to_value(persisted)? != serde_json::to_value(&expected)? {
        return Err(anyhow!(
            "resume post-commit verification failed; recovery required"
        ));
    }
    validate_subscription(&persisted_app, relationship, persisted)?;
    validate_baseline_integrity(&persisted_app, relationship, persisted)?;
    if hash_dir(Path::new(&persisted.local_path))? != persisted.baseline_hash
        || hash_dir(Path::new(&persisted.baseline_path))? != persisted.baseline_hash
    {
        return Err(anyhow!(
            "resume post-commit verification failed; recovery required"
        ));
    }
    Ok(
        json!({"relationship":relationship,"status":"synced","selected_side":side,"recovery_retained":true}),
    )
}
