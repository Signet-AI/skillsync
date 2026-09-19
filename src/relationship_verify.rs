use crate::{filesystem::source_rel, repository, App};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::Path;

fn check(name: &str, status: &str, detail: &str) -> Value {
    json!({"check": name, "status": status, "detail": detail})
}

fn safe_source(source: &str) -> bool {
    if source.contains('@') || source.contains('\\') || Path::new(source).is_absolute() {
        return false;
    }
    repository::normalize(source).is_ok()
}

pub(crate) fn verify(app: &App, relationship: &str) -> Result<Value> {
    if !app.state_path.is_file() {
        return Err(anyhow!(
            "target is not initialized; run init before relationship verification"
        ));
    }
    let Some(s) = app.state.subscriptions.get(relationship) else {
        return Err(anyhow!("subscription relationship not found"));
    };
    let mut checks = Vec::new();
    let mut blockers = Vec::new();
    let source_ok = safe_source(&s.source);
    let source_path_ok = source_rel(&s.source_path).is_ok();
    let identity_ok = source_ok
        && source_path_ok
        && relationship == crate::relationship_key(&s.source, &s.source_path)
        && s.baseline_source == s.source
        && s.baseline_source_path == s.source_path;
    checks.push(check(
        "relationship_identity",
        if identity_ok { "verified" } else { "mismatch" },
        "canonical relationship and portable source identity",
    ));
    if !identity_ok {
        blockers.push("relationship_identity".to_string());
    }

    let authoritative = crate::conflicts::validate_subscription(app, relationship, s).is_ok();
    checks.push(check(
        "subscription_invariants",
        if authoritative { "verified" } else { "blocked" },
        "authoritative persisted subscription validation",
    ));
    if !authoritative {
        blockers.push("subscription_invariants".to_string());
    }
    let baseline = crate::conflicts::validate_baseline_integrity(app, relationship, s).is_ok();
    checks.push(check(
        "baseline_integrity",
        if baseline { "verified" } else { "blocked" },
        "canonical baseline and recorded hash",
    ));
    if !baseline {
        blockers.push("baseline_integrity".to_string());
    }

    let conflict_ok = if s.status == "conflict" {
        crate::conflicts::show(app, relationship).is_ok()
    } else {
        s.recovery_path.is_none()
    };
    checks.push(check(
        "conflict_recovery_evidence",
        if conflict_ok { "verified" } else { "blocked" },
        "strict immutable conflict evidence validation",
    ));
    if !conflict_ok {
        blockers.push("conflict_recovery_evidence".to_string());
    }
    let preflight_blockers = crate::state_stage::relationship_preflight(app, s);
    let preflight_ok = preflight_blockers.is_empty();
    checks.push(check(
        "state_stage_preflight",
        if preflight_ok { "verified" } else { "blocked" },
        "authoritative non-activating state-stage preflight derived from current state",
    ));
    blockers.extend(preflight_blockers);
    blockers.sort();
    blockers.dedup();
    let status = if blockers.iter().any(|x| x == "relationship_identity") {
        "mismatch"
    } else if blockers.is_empty() {
        "verified"
    } else {
        "blocked"
    };
    Ok(json!({
        "format":"skillsync-state-relationship-verification","version":1,"non_activating":true,
        "relationship":relationship,"identity":{"source":if source_ok { "portable_repository" } else { "unsafe_source" },"source_path":if source_path_ok { "portable_relative" } else { "unsafe_path" },"skill":s.skill,"branch":s.branch,"branch_policy":s.branch_policy},
        "status":status,"checks":checks,"blockers":blockers,"would_change":[]
    }))
}
