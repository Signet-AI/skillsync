use crate::{
    filesystem::StateLock,
    state_boundary::{self, Bundle},
    App,
};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::Path;

pub(crate) fn apply_sets(
    app: &mut App,
    from: &Path,
    yes: bool,
    _lock: &StateLock,
) -> Result<Value> {
    if !yes {
        return Err(anyhow!("confirmation required: pass --yes to apply sets"));
    }
    let bundle = state_boundary::validate_bundle(from)?;
    reject_non_set_records(&bundle)?;
    let mut statuses = Vec::new();
    let mut incoming_sets = Vec::new();
    for (name, incoming) in &bundle.sets {
        let members = incoming
            .members
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        if let Some(existing) = app.state.sets.get(name) {
            if existing.members == members {
                statuses.push(serde_json::json!({"set": name, "status": "already_present"}));
            } else {
                return Err(anyhow!("set conflict: {name}"));
            }
        } else {
            incoming_sets.push((name.clone(), members));
            statuses.push(serde_json::json!({"set": name, "status": "added"}));
        }
    }
    let previous_state = app.state.clone();
    for (name, members) in incoming_sets {
        app.state.sets.insert(name, crate::SkillSet { members });
    }
    if statuses.iter().any(|x| x["status"] == "added") {
        if let Err(error) = app.save() {
            app.state = previous_state;
            return Err(error).context("persist set state; state unchanged on disk");
        }
    }
    Ok(serde_json::json!({"sets": statuses}))
}

fn reject_non_set_records(bundle: &Bundle) -> Result<()> {
    for (label, count) in [
        ("subscriptions", bundle.subscriptions.len()),
        ("publications", bundle.publications.len()),
        ("pending_publications", bundle.pending_publications.len()),
        ("local_adoptions", bundle.local_adoptions.len()),
    ] {
        if count != 0 {
            return Err(anyhow!("sets-only apply rejects non-set records: {label}"));
        }
    }
    Ok(())
}
