use crate::{
    filesystem::{
        assert_no_symlink_path, atomic, canonicalize_path_with_missing, read_regular_file,
    },
    state_boundary::{self, Bundle},
    App,
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    library: String,
    state_version: u32,
    initialized: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordKind {
    Subscription,
    Publication,
    PendingPublication,
    Set,
    LocalAdoption,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Classification {
    Ready,
    AlreadyPresent,
    Conflict,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageRecord {
    kind: RecordKind,
    key: String,
    skill: String,
    classification: Classification,
    derived_local_path: Option<String>,
    observed: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagePlan {
    format: String,
    version: u32,
    non_activating: bool,
    bundle_hash: String,
    target: Target,
    records: Vec<StageRecord>,
    activation: String,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn classify(present: bool, equivalent: bool) -> Classification {
    if !present {
        Classification::Ready
    } else if equivalent {
        Classification::AlreadyPresent
    } else {
        Classification::Conflict
    }
}
fn safe_plan_path(path: &Path) -> Result<PathBuf> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(anyhow!("plan destination is a symlink"));
        }
        if !metadata.is_file() {
            return Err(anyhow!("plan destination is not a regular file"));
        }
    }
    let resolved = canonicalize_path_with_missing(path)?;
    let parent = resolved
        .parent()
        .ok_or_else(|| anyhow!("plan has no parent"))?;
    assert_no_symlink_path(parent, Path::new("."))?;
    if resolved.exists() && !fs::symlink_metadata(&resolved)?.is_file() {
        return Err(anyhow!("plan destination is not a regular file"));
    }
    Ok(resolved)
}
fn validate_record_semantics(record: &StageRecord) -> Result<()> {
    let (map, label) = match &record.kind {
        RecordKind::Subscription => ("subscriptions", "subscription"),
        RecordKind::Publication => ("publications", "publication"),
        RecordKind::PendingPublication => ("pending_publications", "pending publication"),
        RecordKind::Set => ("sets", "set"),
        RecordKind::LocalAdoption => ("local_adoptions", "local adoption"),
    };
    let mut bundle = json!({
        "format": "skillsync-state-metadata",
        "version": 1,
        "metadata_only": true,
        "subscriptions": {},
        "publications": {},
        "pending_publications": {},
        "sets": {},
        "local_adoptions": {},
    });
    bundle[map][&record.key] = record.observed.clone();
    state_boundary::validate_bundle_bytes(&serde_json::to_vec(&bundle)?)?;
    if matches!(record.kind, RecordKind::Set) {
        if record.skill != record.key {
            return Err(anyhow!("{label} skill does not match identity"));
        }
    } else if record.observed.get("skill").and_then(Value::as_str) != Some(&record.skill) {
        return Err(anyhow!("{label} skill does not match record"));
    }
    Ok(())
}

fn validate_plan(plan: &StagePlan) -> Result<()> {
    if plan.format != "skillsync-state-stage-plan" || plan.version != 1 {
        return Err(anyhow!("unsupported stage plan format or version"));
    }
    if !plan.non_activating || plan.activation != "not_supported" {
        return Err(anyhow!("stage plan activation mismatch"));
    }
    if plan.bundle_hash.len() != 64 || !plan.bundle_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!("invalid bundle hash"));
    }
    let library = PathBuf::from(&plan.target.library);
    if !library.is_absolute() || !library.exists() || !library.is_dir() {
        return Err(anyhow!("invalid target library"));
    }
    assert_no_symlink_path(&library, Path::new("."))?;
    let mut identities = std::collections::BTreeSet::new();
    let mut paths = std::collections::BTreeSet::new();
    for record in &plan.records {
        validate_record_semantics(record)?;
        let skill = crate::strict_component(&record.skill, "plan skill name")?;
        if record.key.is_empty()
            || !identities.insert((serde_json::to_string(&record.kind)?, record.key.clone()))
        {
            return Err(anyhow!("duplicate plan record identity"));
        }
        let expected = match record.kind {
            RecordKind::Set => None,
            _ => Some(library.join(skill).display().to_string()),
        };
        if record.derived_local_path != expected {
            return Err(anyhow!("forged derived local path"));
        }
        if let Some(path) = &record.derived_local_path {
            if !paths.insert(path.clone()) {
                return Err(anyhow!("duplicate derived local path"));
            }
        }
        if !record.observed.is_object() {
            return Err(anyhow!("malformed observed record"));
        }
    }
    Ok(())
}

fn record(
    kind: RecordKind,
    key: &str,
    skill: &str,
    incoming: Value,
    current: Option<Value>,
    derived: Option<String>,
) -> StageRecord {
    StageRecord {
        kind,
        key: key.into(),
        skill: skill.into(),
        classification: classify(current.is_some(), current.as_ref() == Some(&incoming)),
        derived_local_path: derived,
        observed: incoming,
    }
}
fn bundle_records(bundle: &Bundle, app: &App) -> Vec<StageRecord> {
    let mut out = Vec::new();
    for (key, s) in &bundle.subscriptions {
        let incoming = serde_json::to_value(s).unwrap();
        let current = app.state.subscriptions.get(key).map(|x| json!({"skill":x.skill,"source":x.source,"branch":x.branch,"source_path":x.source_path,"baseline_hash":x.baseline_hash,"baseline_source":x.baseline_source,"baseline_source_path":x.baseline_source_path,"status":x.status,"conflict_selection":x.conflict_selection,"last_sync":x.last_sync,"update_count":x.update_count,"resolved_commit":x.resolved_commit,"resolved_tree":x.resolved_tree}));
        out.push(record(
            RecordKind::Subscription,
            key,
            &s.skill,
            incoming,
            current,
            Some(app.library.join(&s.skill).display().to_string()),
        ));
    }
    for (key, p) in &bundle.publications {
        let incoming = serde_json::to_value(p).unwrap();
        let current = app
            .state
            .publications
            .get(key)
            .map(|x| serde_json::to_value(x).unwrap());
        out.push(record(
            RecordKind::Publication,
            key,
            &p.skill,
            incoming,
            current,
            Some(app.library.join(&p.skill).display().to_string()),
        ));
    }
    for (key, p) in &bundle.pending_publications {
        let incoming = serde_json::to_value(p).unwrap();
        let current = app
            .state
            .pending_publications
            .get(key)
            .map(|x| serde_json::to_value(&x.publication).unwrap());
        out.push(record(
            RecordKind::PendingPublication,
            key,
            &p.skill,
            incoming,
            current,
            Some(app.library.join(&p.skill).display().to_string()),
        ));
    }
    for (key, s) in &bundle.sets {
        let incoming = serde_json::to_value(s).unwrap();
        let current = app
            .state
            .sets
            .get(key)
            .map(|x| serde_json::to_value(x).unwrap());
        out.push(record(RecordKind::Set, key, key, incoming, current, None));
    }
    for (key, a) in &bundle.local_adoptions {
        let incoming = serde_json::to_value(a).unwrap();
        let current = app.state.local_adoptions.get(key).map(|x| json!({"skill":x.skill,"source_package":x.source_package,"content_hash":x.content_hash,"status":x.status}));
        out.push(record(
            RecordKind::LocalAdoption,
            key,
            &a.skill,
            incoming,
            current,
            Some(app.library.join(&a.skill).display().to_string()),
        ));
    }
    out
}
fn current_record(app: &App, record: &StageRecord) -> Option<Value> {
    match &record.kind {
        RecordKind::Subscription => app.state.subscriptions.get(&record.key).map(|x| json!({"skill":x.skill,"source":x.source,"branch":x.branch,"source_path":x.source_path,"baseline_hash":x.baseline_hash,"baseline_source":x.baseline_source,"baseline_source_path":x.baseline_source_path,"status":x.status,"conflict_selection":x.conflict_selection,"last_sync":x.last_sync,"update_count":x.update_count,"resolved_commit":x.resolved_commit,"resolved_tree":x.resolved_tree})),
        RecordKind::Publication => app.state.publications.get(&record.key).map(|x| serde_json::to_value(x).unwrap()),
        RecordKind::PendingPublication => app.state.pending_publications.get(&record.key).map(|x| serde_json::to_value(&x.publication).unwrap()),
        RecordKind::Set => app.state.sets.get(&record.key).map(|x| serde_json::to_value(x).unwrap()),
        RecordKind::LocalAdoption => app.state.local_adoptions.get(&record.key).map(|x| json!({"skill":x.skill,"source_package":x.source_package,"content_hash":x.content_hash,"status":x.status})),
    }
}

pub(crate) fn inspect_plan(
    app: Option<&App>,
    plan_path: &Path,
    bundle_path: Option<&Path>,
) -> Result<Value> {
    let destination = safe_plan_path(plan_path)?;
    if !destination.exists() {
        return Err(anyhow!("plan file does not exist"));
    }
    let plan: StagePlan = serde_json::from_slice(&read_regular_file(&destination, None)?)?;
    validate_plan(&plan)?;
    let bundle_bytes = if let Some(path) = bundle_path {
        let bytes = read_regular_file(&safe_plan_path(path)?, None)?;
        if hash(&bytes) != plan.bundle_hash {
            return Err(anyhow!("bundle hash does not match plan"));
        }
        state_boundary::validate_bundle_bytes(&bytes)?;
        Some(bytes)
    } else {
        None
    };
    if let Some(app) = app {
        if !plan.target.initialized
            || plan.target.library != app.library.to_string_lossy()
            || plan.target.state_version != app.state.version
        {
            return Err(anyhow!("stage plan target mismatch"));
        }
        let expected = if let Some(ref bytes) = bundle_bytes {
            let bundle = state_boundary::validate_bundle_bytes(bytes)?;
            bundle_records(&bundle, app)
        } else {
            Vec::new()
        };
        let stale = if !expected.is_empty() {
            expected.len() != plan.records.len()
                || expected
                    .iter()
                    .zip(&plan.records)
                    .any(|(a, b)| serde_json::to_value(a).ok() != serde_json::to_value(b).ok())
        } else {
            plan.records.iter().any(|record| {
                let current = current_record(app, record);
                serde_json::to_value(classify(
                    current.is_some(),
                    current.as_ref() == Some(&record.observed),
                ))
                .ok()
                    != serde_json::to_value(&record.classification).ok()
                    || current.is_some() && current != Some(record.observed.clone())
            })
        };
        let unverifiable = bundle_bytes.is_none()
            && plan.records.iter().any(|record| {
                current_record(app, record).is_none()
                    && serde_json::to_value(&record.classification).ok()
                        != serde_json::to_value(Classification::Ready).ok()
            });
        let status = if unverifiable {
            "stale_or_unverifiable"
        } else if stale {
            "stale"
        } else {
            "fresh"
        };
        return Ok(
            json!({"format":plan.format,"version":plan.version,"status":status,"record_count":plan.records.len(),"target_available":true}),
        );
    }
    Ok(
        json!({"format":plan.format,"version":plan.version,"status":"target_unavailable","record_count":plan.records.len(),"target_available":false}),
    )
}

pub(crate) fn stage(app: &App, from: &Path, plan: &Path) -> Result<Value> {
    if !app.state_path.exists() {
        return Err(anyhow!(
            "target is not initialized; run init before staging"
        ));
    }
    let bytes = read_regular_file(from, None)?;
    let bundle = state_boundary::validate_bundle_bytes(&bytes)?;
    for s in bundle.subscriptions.values() {
        crate::strict_component(&s.skill, "skill name")?;
    }
    for p in bundle
        .publications
        .values()
        .chain(bundle.pending_publications.values())
    {
        crate::strict_component(&p.skill, "skill name")?;
    }
    for a in bundle.local_adoptions.values() {
        crate::strict_component(&a.skill, "skill name")?;
    }
    let destination = safe_plan_path(plan)?;
    let output = StagePlan {
        format: "skillsync-state-stage-plan".into(),
        version: 1,
        non_activating: true,
        bundle_hash: hash(&bytes),
        target: Target {
            library: app.library.display().to_string(),
            state_version: app.state.version,
            initialized: true,
        },
        records: bundle_records(&bundle, app),
        activation: "not_supported".into(),
    };
    let encoded = serde_json::to_vec_pretty(&output)?;
    let validated: StagePlan = serde_json::from_slice(&encoded)?;
    atomic(&destination, &serde_json::to_vec_pretty(&validated)?)?;
    Ok(
        json!({"format":"skillsync-state-stage-plan","version":1,"plan":destination,"record_count":output.records.len()}),
    )
}
