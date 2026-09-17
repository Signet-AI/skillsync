use crate::{
    filesystem::{discover, effective_library_path, resolve_library_path},
    harness, worker_status, App, State,
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub(crate) struct Inventory {
    pub(crate) library: String,
    pub(crate) packages: Vec<PackageView>,
    pub(crate) harness_links: Vec<HarnessHealth>,
    pub(crate) worker: String,
    pub(crate) capabilities: Capabilities,
}

#[derive(Debug, Serialize)]
pub(crate) struct PackageView {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) sources: Vec<SourceView>,
    pub(crate) relationship: RelationshipView,
}

#[derive(Debug, Serialize)]
pub(crate) struct SourceView {
    pub(crate) kind: String,
    pub(crate) key: Option<String>,
    pub(crate) repository: Option<String>,
    pub(crate) source_path: Option<String>,
    pub(crate) status: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RelationshipRecord {
    pub(crate) key: String,
    pub(crate) source_path: Option<String>,
    pub(crate) status: String,
}

#[derive(Debug, Serialize, Default)]
pub(crate) struct RelationshipView {
    pub(crate) subscriptions: Vec<RelationshipRecord>,
    pub(crate) publications: Vec<RelationshipRecord>,
    pub(crate) local_adoptions: Vec<RelationshipRecord>,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub(crate) struct HarnessHealth {
    pub(crate) relationship: String,
    pub(crate) skill: Option<String>,
    pub(crate) set: Option<String>,
    pub(crate) harness_root: String,
    pub(crate) link_path: Option<String>,
    pub(crate) status: String,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Capabilities {
    pub(crate) hermes_autonomous_curation: &'static str,
    pub(crate) harness_discovery: &'static str,
    pub(crate) harness_filtering: &'static str,
    pub(crate) harness_reload: &'static str,
    pub(crate) full_tui: &'static str,
    pub(crate) registry_integration: &'static str,
}

pub(crate) fn query(a: &App) -> Result<Inventory> {
    let mut inventory = query_with(&a.library, &a.state, &a.config)?;
    let mut links = harness::harness_link_health(a)?;
    links.extend(harness::inventory_harness_health(&a.state, &a.library)?);
    inventory.harness_links = serde_json::from_value(serde_json::Value::Array(links))?;
    Ok(inventory)
}

pub(crate) fn query_uninitialized() -> Result<Inventory> {
    let requested = effective_library_path(None);
    let library = resolve_library_path(&requested)?;
    query_with(&library, &State::default(), Path::new("."))
}

fn query_with(library: &Path, state: &State, config: &Path) -> Result<Inventory> {
    let found = discover(library)?;
    let packages = found
        .into_iter()
        .map(|(name, path, relative)| {
            let mut sources = Vec::new();
            let mut subscriptions = Vec::new();
            for (key, record) in &state.subscriptions {
                if record.skill == name && record.source_path == relative {
                    sources.push(SourceView {
                        kind: "subscription".into(),
                        key: Some(key.clone()),
                        repository: Some(record.source.clone()),
                        source_path: Some(record.source_path.clone()),
                        status: Some(record.status.clone()),
                    });
                    subscriptions.push(RelationshipRecord {
                        key: key.clone(),
                        source_path: Some(record.source_path.clone()),
                        status: record.status.clone(),
                    });
                }
            }
            let mut publications = Vec::new();
            for (key, record) in &state.publications {
                if record.skill == name && library.join(&record.skill) == path {
                    publications.push(RelationshipRecord {
                        key: key.clone(),
                        source_path: Some(record.path.clone()),
                        status: record.status.clone(),
                    });
                }
            }
            let mut local_adoptions = Vec::new();
            for (key, record) in &state.local_adoptions {
                if record.skill == name && Path::new(&record.local_path) == path {
                    sources.push(SourceView {
                        kind: "local_adoption".into(),
                        key: Some(key.clone()),
                        repository: None,
                        source_path: Some(record.source_package.clone()),
                        status: Some(record.status.clone()),
                    });
                    local_adoptions.push(RelationshipRecord {
                        key: key.clone(),
                        source_path: Some(record.source_package.clone()),
                        status: record.status.clone(),
                    });
                }
            }
            if sources.is_empty() {
                sources.push(SourceView {
                    kind: "local".into(),
                    key: None,
                    repository: None,
                    source_path: None,
                    status: None,
                });
            }
            PackageView {
                name,
                path: relative,
                sources,
                relationship: RelationshipView {
                    subscriptions,
                    publications,
                    local_adoptions,
                },
            }
        })
        .collect();
    let harness_links = serde_json::from_value(serde_json::Value::Array(
        harness::inventory_harness_health(state, library)?,
    ))
    .context("serialize harness health")?;
    Ok(Inventory {
        library: library.display().to_string(),
        packages,
        harness_links,
        worker: if config == Path::new(".") {
            "stopped".into()
        } else {
            worker_status(config)?.into()
        },
        capabilities: Capabilities {
            hermes_autonomous_curation: "unsupported",
            harness_discovery: "unsupported",
            harness_filtering: "unsupported",
            harness_reload: "unsupported",
            full_tui: "unsupported",
            registry_integration: "unsupported",
        },
    })
}
