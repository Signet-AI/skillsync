use crate::{
    branch_policy::BranchPolicy,
    filesystem::{assert_no_symlink_path, atomic, checked_regular_path, source_rel},
    App,
};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path},
};

fn canonical_source_rel(value: &str) -> Result<String> {
    source_rel(value)?;
    if value == "." {
        return Ok(".".into());
    }
    Ok(Path::new(value)
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bundle {
    pub(crate) format: String,
    pub(crate) version: u32,
    pub(crate) metadata_only: bool,
    pub(crate) subscriptions: BTreeMap<String, Subscription>,
    pub(crate) publications: BTreeMap<String, Publication>,
    pub(crate) pending_publications: BTreeMap<String, Publication>,
    pub(crate) sets: BTreeMap<String, SkillSet>,
    pub(crate) local_adoptions: BTreeMap<String, LocalAdoption>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Subscription {
    #[serde(default)]
    pub(crate) branch_policy: Option<BranchPolicy>,
    pub(crate) skill: String,
    pub(crate) source: String,
    pub(crate) branch: String,
    pub(crate) source_path: String,
    pub(crate) baseline_hash: String,
    pub(crate) baseline_source: String,
    pub(crate) baseline_source_path: String,
    pub(crate) status: String,
    pub(crate) conflict_selection: Option<String>,
    pub(crate) last_sync: u64,
    pub(crate) update_count: u64,
    #[serde(default)]
    pub(crate) resolved_commit: Option<String>,
    #[serde(default)]
    pub(crate) resolved_tree: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Publication {
    pub(crate) skill: String,
    pub(crate) destination: String,
    pub(crate) branch: String,
    pub(crate) path: String,
    pub(crate) approved: bool,
    pub(crate) status: String,
    pub(crate) last_hash: Option<String>,
    pub(crate) last_sync: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillSet {
    pub(crate) members: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalAdoption {
    pub(crate) skill: String,
    pub(crate) source_package: String,
    pub(crate) content_hash: String,
    pub(crate) status: String,
}

fn portable_state(app: &App) -> Result<Bundle> {
    let mut subscriptions = BTreeMap::new();
    for (key, s) in &app.state.subscriptions {
        let source_path = canonical_source_rel(&s.source_path)?;
        validate_remote(&s.source, "source")?;
        crate::strict_component(&s.skill, "subscription skill name")?;
        crate::repository::validate_branch(&s.branch)?;
        if let Some(policy) = &s.branch_policy {
            policy.validate_effective_branch(&s.branch)?;
        }
        validate_hash(&s.baseline_hash, "baseline hash")?;
        validate_object_id(s.resolved_commit.as_deref(), "resolved commit")?;
        validate_object_id(s.resolved_tree.as_deref(), "resolved tree")?;
        validate_remote(&s.baseline_source, "baseline source")?;
        if s.baseline_source != s.source {
            return Err(anyhow!("baseline source does not match source"));
        }
        if canonical_source_rel(&s.baseline_source_path)? != source_path {
            return Err(anyhow!("baseline source path does not match source path"));
        }
        validate_subscription_status(&s.status)?;
        if let Some(selection) = &s.conflict_selection {
            validate_conflict_selection(selection)?;
        }
        if *key != crate::relationship_key(&s.source, &source_path) {
            return Err(anyhow!("subscription key does not match relationship"));
        }
        subscriptions.insert(
            key.clone(),
            Subscription {
                branch_policy: s
                    .branch_policy
                    .clone()
                    .or_else(|| BranchPolicy::migrate_legacy(&s.branch).ok()),
                skill: s.skill.clone(),
                source: s.source.clone(),
                branch: s.branch.clone(),
                source_path,
                baseline_hash: s.baseline_hash.clone(),
                baseline_source: s.baseline_source.clone(),
                baseline_source_path: canonical_source_rel(&s.baseline_source_path)?,
                status: s.status.clone(),
                conflict_selection: s.conflict_selection.clone(),
                last_sync: s.last_sync,
                update_count: s.update_count,
                resolved_commit: s.resolved_commit.clone(),
                resolved_tree: s.resolved_tree.clone(),
            },
        );
    }
    let publications = app
        .state
        .publications
        .iter()
        .map(|(key, p)| checked_publication(key, p).map(|p| (key.clone(), p)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let pending_publications = app
        .state
        .pending_publications
        .iter()
        .map(|(key, p)| checked_publication(key, &p.publication).map(|p| (key.clone(), p)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut sets = BTreeMap::new();
    for (key, s) in &app.state.sets {
        crate::set_member_name(&format!("library:{}", key))
            .or_else(|_| crate::strict_component(key, "set name").map(|_| key.clone()))?;
        for m in &s.members {
            crate::set_member_name(m)?;
        }
        sets.insert(
            key.clone(),
            SkillSet {
                members: s.members.iter().cloned().collect(),
            },
        );
    }
    let mut local_adoptions = BTreeMap::new();
    for (key, a) in &app.state.local_adoptions {
        let skill = crate::strict_component(&a.skill, "skill name")?;
        let source_package = canonical_source_rel(&a.source_package)?;
        if *key != format!("local:{skill}") && *key != format!("local:path:{source_package}") {
            return Err(anyhow!("local adoption key does not match skill"));
        }
        local_adoptions.insert(
            key.clone(),
            LocalAdoption {
                skill: a.skill.clone(),
                source_package,
                content_hash: a.content_hash.clone(),
                status: a.status.clone(),
            },
        );
    }
    Ok(Bundle {
        format: "skillsync-state-metadata".into(),
        version: 1,
        metadata_only: true,
        subscriptions,
        publications,
        pending_publications,
        sets,
        local_adoptions,
    })
}
fn validate_text(s: &str, label: &str) -> Result<()> {
    if s.is_empty() || s.chars().any(|c| c.is_control()) {
        Err(anyhow!("invalid {label}"))
    } else {
        Ok(())
    }
}
fn checked_publication(key: &str, p: &crate::Publication) -> Result<Publication> {
    validate_text(&p.skill, "skill")?;
    validate_remote(&p.destination, "destination")?;
    crate::repository::validate_branch(&p.branch)?;
    let path = canonical_source_rel(&p.path)?;
    if key != crate::publication_key(&p.skill, &p.destination, &p.branch, &path) {
        return Err(anyhow!("publication key does not match identity"));
    }
    if let Some(h) = &p.last_hash {
        validate_hash(h, "publication hash")?;
    }
    validate_text(&p.status, "status")?;
    Ok(Publication {
        skill: p.skill.clone(),
        destination: p.destination.clone(),
        branch: p.branch.clone(),
        path,
        approved: p.approved,
        status: p.status.clone(),
        last_hash: p.last_hash.clone(),
        last_sync: p.last_sync,
    })
}
fn validate_remote(s: &str, label: &str) -> Result<()> {
    validate_text(s, label)?;
    if s.starts_with("git@") && s.contains(':') {
        return Err(anyhow!("credential-bearing {label}"));
    }
    if let Some((scheme, rest)) = s.split_once("://") {
        if !matches!(scheme, "http" | "https" | "ssh") {
            return Err(anyhow!("unsupported remote scheme for {label}"));
        }
        if rest.split('/').next().is_some_and(|x| x.contains('@')) {
            return Err(anyhow!("credential-bearing {label}"));
        }
    } else {
        return Err(anyhow!("nonportable {label}"));
    }
    if s.starts_with("/") {
        return Err(anyhow!("credential-bearing {label}"));
    }
    Ok(())
}
fn validate_hash(s: &str, label: &str) -> Result<()> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Err(anyhow!("invalid {label}"))
    } else {
        Ok(())
    }
}
fn validate_object_id(value: Option<&str>, label: &str) -> Result<()> {
    if let Some(s) = value {
        if s.len() != 40 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(anyhow!("invalid {label}"));
        }
    }
    Ok(())
}

fn validate_subscription_status(s: &str) -> Result<()> {
    if matches!(
        s,
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
        Ok(())
    } else {
        Err(anyhow!("invalid subscription status: {s}"))
    }
}
fn validate_conflict_selection(s: &str) -> Result<()> {
    if matches!(s, "local" | "incoming") {
        Ok(())
    } else {
        Err(anyhow!("invalid conflict selection: {s}"))
    }
}
fn validate_branch(s: &str) -> Result<()> {
    crate::repository::validate_branch(s)
}
pub(crate) fn validate_bundle_bytes(bytes: &[u8]) -> Result<Bundle> {
    let mut b: Bundle =
        serde_json::from_slice(bytes).map_err(|e| anyhow!("invalid state bundle: {e}"))?;
    if b.format != "skillsync-state-metadata" || b.version != 1 || !b.metadata_only {
        return Err(anyhow!("unsupported or non-metadata state bundle"));
    }
    for (key, s) in &mut b.subscriptions {
        validate_text(&s.skill, "skill")?;
        validate_remote(&s.source, "source")?;
        crate::repository::validate_branch(&s.branch)?;
        if let Some(policy) = &s.branch_policy {
            policy.validate_effective_branch(&s.branch)?;
        } else {
            s.branch_policy = Some(BranchPolicy::migrate_legacy(&s.branch)?);
        }
        canonical_source_rel(&s.source_path)?;
        if key != &crate::relationship_key(&s.source, &canonical_source_rel(&s.source_path)?) {
            return Err(anyhow!("subscription key does not match relationship"));
        }
        s.source_path = canonical_source_rel(&s.source_path)?;
        s.baseline_source_path = canonical_source_rel(&s.baseline_source_path)?;
        validate_remote(&s.baseline_source, "baseline source")?;
        if s.baseline_source != s.source {
            return Err(anyhow!("baseline source does not match source"));
        }
        if canonical_source_rel(&s.baseline_source_path)? != canonical_source_rel(&s.source_path)? {
            return Err(anyhow!("baseline source path does not match source path"));
        }
        validate_hash(&s.baseline_hash, "baseline hash")?;
        validate_object_id(s.resolved_commit.as_deref(), "resolved commit")?;
        validate_object_id(s.resolved_tree.as_deref(), "resolved tree")?;
        validate_subscription_status(&s.status)?;
        if let Some(x) = &s.conflict_selection {
            validate_conflict_selection(x)?;
        }
    }
    for (kind, entries) in [
        ("publications", &b.publications),
        ("pending publications", &b.pending_publications),
    ] {
        for (key, p) in entries {
            validate_text(&p.skill, "skill")?;
            validate_remote(&p.destination, "destination")?;
            crate::repository::validate_branch(&p.branch)?;
            let path = canonical_source_rel(&p.path)?;
            if key != &crate::publication_key(&p.skill, &p.destination, &p.branch, &path) {
                return Err(anyhow!("{kind} key does not match identity"));
            }
        }
    }
    for entries in [&mut b.publications, &mut b.pending_publications] {
        for p in entries.values_mut() {
            validate_text(&p.skill, "skill")?;
            validate_remote(&p.destination, "destination")?;
            validate_branch(&p.branch)?;
            p.path = canonical_source_rel(&p.path)?;
            if let Some(h) = &p.last_hash {
                validate_hash(h, "publication hash")?;
            }
            validate_text(&p.status, "status")?;
        }
    }
    for (key, s) in &b.sets {
        crate::strict_component(key, "set name")?;
        let mut members = BTreeSet::new();
        for m in &s.members {
            crate::set_member_name(m)?;
            if !members.insert(m) {
                return Err(anyhow!("duplicate set member: {m}"));
            }
        }
    }
    for (key, a) in &mut b.local_adoptions {
        validate_text(&a.skill, "skill")?;
        crate::strict_component(&a.skill, "skill name")?;
        a.source_package = canonical_source_rel(&a.source_package)?;
        if key != &format!("local:{}", a.skill)
            && key != &format!("local:path:{}", a.source_package)
        {
            return Err(anyhow!("local adoption key does not match skill"));
        }
        validate_hash(&a.content_hash, "content hash")?;
        validate_text(&a.status, "status")?;
    }
    Ok(b)
}
pub(crate) fn validate_bundle(path: &Path) -> Result<Bundle> {
    checked_regular_path(path, "state bundle")?;
    assert_no_symlink_path(
        path.parent().unwrap_or(Path::new(".")),
        Path::new(path.file_name().unwrap()),
    )?;
    validate_bundle_bytes(&fs::read(path)?)
}
pub(crate) fn export(app: &App, out: &Path) -> Result<Value> {
    if out.exists() {
        checked_regular_path(out, "export destination")?;
    }
    let b = portable_state(app)?;
    let bytes = serde_json::to_vec_pretty(&b)?;
    if let Some(p) = out.parent() {
        fs::create_dir_all(p)?;
    }
    atomic(out, &bytes)?;
    Ok(serde_json::json!({"format":b.format,"version":b.version,"metadata_only":true,"out":out}))
}
pub(crate) fn inspect(from: &Path) -> Result<Value> {
    serde_json::to_value(validate_bundle(from)?).map_err(Into::into)
}
