use anyhow::{anyhow, Result};
use serde_json::json;
use std::{
    fs,
    path::{Component, PathBuf},
};

use crate::filesystem::{
    assert_no_symlink_path, files, hash_dir, manifest_name, safe, strict_component,
    validate_state_path,
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
        || fs::canonicalize(&local)? != local
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
        || fs::canonicalize(&baseline)? != baseline
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
        validate_baseline_integrity(a, relationship, subscription)?;
        let recovery_raw = subscription
            .recovery_path
            .as_ref()
            .ok_or_else(|| anyhow!("conflict relationship has no recovery path: {relationship}"))?;
        let recovery = PathBuf::from(recovery_raw);
        validate_state_path(&a.recovery, &recovery, "recovery")?;
        let relative = recovery
            .strip_prefix(&a.recovery)
            .map_err(|_| anyhow!("recovery path escaped configured recovery root"))?;
        if relative.components().count() != 1
            || !relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(anyhow!(
                "conflict recovery must be a direct child of the recovery root"
            ));
        }
        let name = relative
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with(&format!("{relationship}-")) {
            return Err(anyhow!(
                "recovery path does not belong to subscription relationship: {relationship}"
            ));
        }
        let local = recovery.join("local");
        let incoming = recovery.join("incoming");
        for (label, package) in [("local", &local), ("incoming", &incoming)] {
            validate_state_path(&a.recovery, package, "recovery package")?;
            let metadata = fs::symlink_metadata(package)
                .map_err(|_| anyhow!("recovery is missing {label} package"))?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(anyhow!(
                    "recovery {label} package must be a regular directory"
                ));
            }
            assert_no_symlink_path(&a.recovery, package.strip_prefix(&a.recovery)?)?;
            if files(package)?.is_empty() || manifest_name(package)? != subscription.skill {
                return Err(anyhow!(
                    "recovery {label} package manifest does not match subscription"
                ));
            }
        }
        let live = PathBuf::from(&subscription.local_path);
        let live_hash = hash_dir(&live)?;
        let local_hash = hash_dir(&local)?;
        let stale = live_hash != local_hash;
        conflicts.push(json!({"relationship": relationship, "skill": subscription.skill,
            "status": subscription.status, "recovery_path": recovery,
            "local_hash": local_hash, "incoming_hash": hash_dir(&incoming)?,
            "live_hash": live_hash, "stale": stale,
            "resolution": "explicit resume is not yet available: state lacks immutable conflict snapshot"}));
    }
    Ok(json!({"conflicts": conflicts, "count": conflicts.len()}))
}
