use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    filesystem::{checked_regular_path, lock_is_contended, open_advisory_lock, WorkerLease},
    repository, App,
};

pub(crate) static WORKER_STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn worker_status(config: &Path) -> Result<&'static str> {
    let path = config.join("worker.active");
    if !checked_regular_path(&path, "worker status")? {
        return Ok("stopped");
    }
    let file = open_advisory_lock(config, "worker.active", false)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock()?;
            Ok("stopped")
        }
        Err(error) if lock_is_contended(&error) => Ok("running"),
        Err(error) => Err(error).context("inspect worker status"),
    }
}

fn now() -> u64 {
    #[cfg(feature = "test-hooks")]
    if let Ok(value) = std::env::var("SKILLSYNC_TEST_NOW") {
        if let Ok(value) = value.parse() {
            return value;
        }
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn retry_delay(attempt: u64) -> u64 {
    const CAP: u64 = 86_400;
    let exponent = attempt.saturating_sub(1).min(63);
    60_u64.checked_shl(exponent as u32).unwrap_or(CAP).min(CAP)
}

fn pending_due(pending: &crate::PendingPublication, current: u64) -> bool {
    pending.next_attempt_at == 0 || current >= pending.next_attempt_at
}

fn safe_error(status: &str) -> &'static str {
    match status {
        "authentication_required" => "repository authentication required",
        "offline" => "repository unavailable offline",
        "permission_denied" => "repository access denied",
        "source_missing" => "source unavailable",
        "branch_missing" => "tracked branch missing",
        "package_missing" => "skill package missing",
        "invalid_source" => "invalid repository source",
        _ => "repository operation failed",
    }
}

fn cycle_summary(results: &[serde_json::Value]) -> serde_json::Value {
    let mut subscriptions = [0_u64; 4];
    let mut publications = [0_u64; 4];
    let mut by_status = std::collections::BTreeMap::<String, u64>::new();
    for result in results {
        let status = result
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        *by_status.entry(status.to_owned()).or_default() += 1;
        let scheduled =
            result.get("queue").and_then(serde_json::Value::as_str) == Some("scheduled");
        let counters = if result.get("relationship").is_some() && !scheduled {
            &mut subscriptions
        } else {
            &mut publications
        };
        if scheduled {
            counters[3] += 1;
        } else {
            counters[0] += 1;
            if matches!(status, "synced" | "customized" | "no_change" | "published") {
                counters[1] += 1;
            } else {
                counters[2] += 1;
            }
        }
    }
    serde_json::json!({
        "total_results": results.len(),
        "subscriptions": {"attempted": subscriptions[0], "succeeded": subscriptions[1], "failed": subscriptions[2], "scheduled": subscriptions[3]},
        "publications": {"attempted": publications[0], "succeeded": publications[1], "failed": publications[2], "scheduled": publications[3]},
        "by_status": by_status,
    })
}

pub(crate) fn sync_all(a: &mut App, continue_on_error: bool) -> Result<serde_json::Value> {
    let cycle_now = now();
    let keys = a.state.subscriptions.keys().cloned().collect::<Vec<_>>();
    let mut results = vec![];
    for key in keys {
        match repository::update_one(a, &key) {
            Ok(result) => results.push(result),
            Err(error) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(error),
            Err(error) if !continue_on_error => return Err(error),
            Err(error) => {
                let error_text = error.to_string();
                let status = repository::status_for_error(&error_text);
                if let Some(subscription) = a.state.subscriptions.get_mut(&key) {
                    subscription.status = status.into();
                    subscription.last_sync = cycle_now;
                }
                a.save()?;
                results.push(
                    serde_json::json!({"relationship": key, "status": status, "error": safe_error(status)}),
                );
            }
        }
    }
    let pending = a
        .state
        .pending_publications
        .iter()
        .map(|(key, pending)| (key.clone(), pending.publication.clone()))
        .collect::<Vec<_>>();
    let pending_keys = pending
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for (key, intent) in pending {
        if let Some(pending) = a.state.pending_publications.get(&key) {
            if !pending_due(pending, cycle_now) {
                results.push(serde_json::json!({"skill": intent.skill, "relationship": key, "status": "pending_push", "queue": "scheduled", "next_attempt_at": pending.next_attempt_at}));
                continue;
            }
        }
        let previous = a
            .state
            .publications
            .get(&key)
            .cloned()
            .or_else(|| Some(intent.clone()));
        match repository::publish_to_repo(a, &intent.skill, &intent.destination, previous.as_ref())
        {
            Ok(result) => results.push(result),
            Err(error) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(error),
            Err(error) if !continue_on_error => return Err(error),
            Err(error) => {
                let error_text = error.to_string();
                let status = repository::status_for_error(&error_text);
                if let Some(pending) = a.state.pending_publications.get_mut(&key) {
                    let attempted_at = cycle_now;
                    pending.attempt_count = pending.attempt_count.saturating_add(1);
                    pending.last_attempt_at = attempted_at;
                    pending.next_attempt_at =
                        attempted_at.saturating_add(retry_delay(pending.attempt_count));
                    pending.last_error_status = Some(status.into());
                    pending.publication.status = status.into();
                    pending.publication.last_sync = attempted_at;
                }
                if let Some(publication) = a.state.publications.get_mut(&key) {
                    publication.status = status.into();
                    publication.last_sync = cycle_now;
                }
                a.save()?;
                results.push(serde_json::json!({"skill": intent.skill, "relationship": key, "status": status, "error": safe_error(status)}));
            }
        }
    }
    let publications = a
        .state
        .publications
        .iter()
        .filter(|(key, publication)| publication.approved && !pending_keys.contains(*key))
        .map(|(key, publication)| {
            (
                key.clone(),
                publication.skill.clone(),
                publication.destination.clone(),
                publication.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (key, skill, destination, previous) in publications {
        match repository::publish_to_repo(a, &skill, &destination, Some(&previous)) {
            Ok(result) => results.push(result),
            Err(error) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(error),
            Err(error) => {
                let error_text = error.to_string();
                let status = repository::status_for_error(&error_text);
                if let Some(publication) = a.state.publications.get_mut(&key) {
                    publication.status = status.into();
                    publication.last_sync = cycle_now;
                }
                a.save()?;
                results.push(
                    serde_json::json!({"skill": skill, "status": status, "error": safe_error(status)}),
                );
            }
        }
    }
    Ok(serde_json::json!({"results": results}))
}

fn wait_worker_interval(stop: &AtomicBool, interval: u64) {
    let deadline = Instant::now() + Duration::from_secs(interval);
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn run_worker_locked(
    a: &mut App,
    once: bool,
    interval: u64,
) -> Result<serde_json::Value> {
    if interval == 0 {
        return Err(anyhow!("worker interval must be greater than zero seconds"));
    }
    WORKER_STOP_REQUESTED.store(false, Ordering::Relaxed);
    if once {
        let _worker_lease = WorkerLease::acquire(&a.config)?;
        let sync = sync_all(a, true)?;
        let results = sync
            .get("results")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        return Ok(
            serde_json::json!({"worker":"completed","results":results,"cycle_summary":cycle_summary(results.as_array().unwrap_or(&vec![]))}),
        );
    }
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        WORKER_STOP_REQUESTED.store(true, Ordering::Relaxed);
        signal_stop.store(true, Ordering::Relaxed);
    })
    .context("install Ctrl-C handler for worker")?;
    let _worker_lease = WorkerLease::acquire(&a.config)?;
    let mut cycles = 0_u64;
    let mut cancelled = false;
    while !stop.load(Ordering::Relaxed) {
        match sync_all(a, true) {
            Ok(sync) => {
                cycles += 1;
                let count = sync
                    .get("results")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                eprintln!("skillsync worker cycle {cycles} complete ({count} result(s))");
            }
            Err(_error) if stop.load(Ordering::Relaxed) => {
                cancelled = true;
                break;
            }
            Err(error) => {
                cycles += 1;
                eprintln!("skillsync worker cycle {cycles} failed: {error}");
            }
        }
        wait_worker_interval(&stop, interval);
    }
    if stop.load(Ordering::Relaxed) {
        cancelled = true;
    }
    Ok(serde_json::json!({"worker":"stopped","cycles":cycles,"cancelled":cancelled}))
}
