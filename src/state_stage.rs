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
        let current = app.state.subscriptions.get(key).map(|x| json!({"skill":x.skill,"source":x.source,"branch":x.branch,"source_path":x.source_path,"baseline_hash":x.baseline_hash,"baseline_source":x.baseline_source,"baseline_source_path":x.baseline_source_path,"status":x.status,"conflict_selection":x.conflict_selection,"last_sync":x.last_sync,"update_count":x.update_count}));
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
