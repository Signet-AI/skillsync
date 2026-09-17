use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    path::{Component, PathBuf},
};

use crate::filesystem::{
    assert_no_symlink_path, canonicalize_path, hash_dir, manifest_name, safe, strict_component, validate_state_path,
};
use crate::{relationship_key, App};

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
    ) {
        return Err(anyhow!(
            "invalid subscription status: {}",
            subscription.status
        ));
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
    if !meta.is_dir() || meta.file_type().is_symlink() || canonicalize_path(&recovery)? != recovery {
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
