use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn path_adoption_key(source_package: &str) -> String {
    format!("local:path:{source_package}")
}

#[derive(Debug, Serialize)]
pub(crate) struct DiscoverReport {
    pub schema_version: u32,
    pub mode: &'static str,
    pub roots: Vec<RootReport>,
    pub packages: Vec<PackageReport>,
    pub occurrences: Vec<OccurrenceReport>,
    pub proposals: Vec<ProposalReport>,
    pub diagnostics: Vec<DiagnosticReport>,
    pub capabilities: BTreeMap<&'static str, &'static str>,
}
#[derive(Debug, Serialize)]
pub(crate) struct RootReport {
    pub kind: String,
    pub path: String,
    pub status: String,
}
#[derive(Debug, Serialize)]
pub(crate) struct PackageReport {
    pub name: String,
    pub path: String,
    pub status: String,
}
#[derive(Debug, Serialize)]
pub(crate) struct OccurrenceReport {
    pub name: String,
    pub root: String,
    pub path: String,
    pub status: String,
}
#[derive(Debug, Serialize)]
pub(crate) struct ProposalReport {
    pub kind: &'static str,
    pub path: String,
    pub requires_approval: bool,
    pub destructive: bool,
}
#[derive(Debug, Serialize, Clone)]
pub(crate) struct DiagnosticReport {
    pub code: &'static str,
    pub path: String,
    pub detail: &'static str,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdoptionPlan {
    pub format: String,
    pub version: u32,
    pub library: String,
    pub operations: Vec<AdoptionOperation>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdoptionOperation {
    pub skill: String,
    pub source_package: String,
    pub source_path: String,
    pub destination: String,
    pub content_hash: String,
}

pub(crate) fn make_plan(library: &Path, state: &crate::State) -> Result<AdoptionPlan> {
    let report = discover(library, state)?;
    let mut operations = Vec::new();
    for proposal in report.proposals {
        let package = library.join(&proposal.path);
        operations.push(AdoptionOperation {
            skill: crate::filesystem::manifest_name(&package)?,
            source_package: proposal.path,
            source_path: library.display().to_string(),
            destination: package.display().to_string(),
            content_hash: crate::filesystem::hash_dir(&package)?,
        });
    }
    operations.sort_by(|a, b| (&a.source_package, &a.skill).cmp(&(&b.source_package, &b.skill)));
    Ok(AdoptionPlan {
        format: "skillsync-onboarding-adoption-plan".into(),
        version: 2,
        library: library.display().to_string(),
        operations,
    })
}

pub(crate) fn write_plan(
    library: &Path,
    state: &crate::State,
    out: &Path,
) -> Result<serde_json::Value> {
    if out.exists() {
        crate::filesystem::checked_regular_path(out, "plan destination")?;
    }
    let plan = make_plan(library, state)?;
    crate::filesystem::atomic(out, &serde_json::to_vec_pretty(&plan)?)?;
    Ok(
        serde_json::json!({"format":plan.format,"version":plan.version,"operations":plan.operations.len(),"out":out}),
    )
}

pub(crate) fn apply_plan(
    a: &mut crate::App,
    plan_path: &Path,
    yes: bool,
) -> Result<serde_json::Value> {
    if !yes {
        return Err(anyhow::anyhow!(
            "confirmation required: pass --yes to apply onboarding plan"
        ));
    }
    crate::filesystem::checked_regular_path(plan_path, "onboarding plan")?;
    let plan: AdoptionPlan = serde_json::from_slice(&fs::read(plan_path)?)?;
    if plan.format != "skillsync-onboarding-adoption-plan"
        || !(plan.version == 1 || plan.version == 2)
    {
        return Err(anyhow::anyhow!("unsupported onboarding plan"));
    }
    if plan.version == 1 && plan.operations.len() != 1 {
        return Err(anyhow::anyhow!(
            "onboarding apply requires exactly one operation"
        ));
    }
    if plan.version == 2 && plan.operations.is_empty() {
        return Err(anyhow::anyhow!(
            "onboarding v2 plan must contain operations"
        ));
    }
    if plan.version == 2 {
        let mut sources = BTreeSet::new();
        let mut destinations = BTreeSet::new();
        for op in &plan.operations {
            if !sources.insert(op.source_package.clone())
                || !destinations.insert(op.destination.clone())
            {
                return Err(anyhow::anyhow!(
                    "onboarding v2 plan contains duplicate operation identity"
                ));
            }
        }
    }
    let op = &plan.operations[0];
    let skill = crate::filesystem::strict_component(&op.skill, "adoption skill name")?;
    let destination = a.library.join(&op.source_package);
    if Path::new(&plan.library) != a.library
        || Path::new(&op.destination) != destination
        || Path::new(&op.source_path) != a.library
    {
        return Err(anyhow::anyhow!(
            "onboarding plan target does not match initialized library"
        ));
    }
    crate::filesystem::source_rel(&op.source_package)?;
    crate::filesystem::assert_no_symlink_path(&a.library, Path::new(&op.source_package))?;
    let source_package = a.library.join(&op.source_package);
    if !source_package.is_dir() || crate::filesystem::manifest_name(&source_package)? != skill {
        return Err(anyhow::anyhow!(
            "adoption source is not the canonical unmanaged package"
        ));
    }
    let current_hash = crate::filesystem::hash_dir(&source_package)?;
    if current_hash != op.content_hash {
        return Err(anyhow::anyhow!("adoption source hash is stale"));
    }
    let key = format!("local:{skill}");
    if plan.version == 1 {
        if let Some(existing) = a.state.local_adoptions.get(&key) {
            if existing.content_hash == current_hash {
                return Ok(serde_json::json!({"skill":skill,"status":"already_adopted"}));
            }
            return Err(anyhow::anyhow!(
                "package already has a managed relationship"
            ));
        }
    }
    if plan.version == 1
        && discover(&a.library, &a.state)?
            .packages
            .iter()
            .any(|package| package.path == op.source_package && package.status == "managed")
    {
        return Err(anyhow::anyhow!(
            "package already has a managed relationship"
        ));
    }
    if plan.version == 1
        && (a.state.subscriptions.values().any(|x| x.skill == skill)
            || a.state.publications.values().any(|x| x.skill == skill)
            || a.state.harness_links.values().any(|x| x.skill == skill)
            || a.state
                .harness_sets
                .values()
                .any(|x| x.members.contains(&skill)))
    {
        return Err(anyhow::anyhow!(
            "package already has a managed relationship"
        ));
    }
    if plan.version == 1 {
        let previous = a.state.clone();
        a.state.local_adoptions.insert(
            key,
            crate::LocalAdoption {
                skill: skill.clone(),
                source_path: a.library.display().to_string(),
                source_package: op.source_package.clone(),
                content_hash: current_hash,
                local_path: destination.display().to_string(),
                status: "adopted".into(),
            },
        );
        if let Err(error) = a.save() {
            a.state = previous;
            return Err(error.context("persist onboarding adoption state"));
        }
        return Ok(
            serde_json::json!({"skill":skill,"status":"adopted","canonical_path":destination}),
        );
    }

    let mut prepared = a.state.clone();
    let mut statuses = Vec::new();
    for operation in &plan.operations {
        let skill = crate::filesystem::strict_component(&operation.skill, "adoption skill name")?;
        let destination = a.library.join(&operation.source_package);
        if Path::new(&plan.library) != a.library
            || Path::new(&operation.destination) != destination
            || Path::new(&operation.source_path) != a.library
        {
            return Err(anyhow::anyhow!(
                "onboarding plan target does not match initialized library"
            ));
        }
        crate::filesystem::source_rel(&operation.source_package)?;
        crate::filesystem::assert_no_symlink_path(
            &a.library,
            Path::new(&operation.source_package),
        )?;
        let source_package = a.library.join(&operation.source_package);
        if !source_package.is_dir() || crate::filesystem::manifest_name(&source_package)? != skill {
            return Err(anyhow::anyhow!(
                "adoption source is not the canonical unmanaged package"
            ));
        }
        let hash = crate::filesystem::hash_dir(&source_package)?;
        if hash != operation.content_hash {
            return Err(anyhow::anyhow!("adoption source hash is stale"));
        }
        let key = path_adoption_key(&operation.source_package);
        if let Some(existing) = prepared.local_adoptions.get(&key) {
            if existing.content_hash == hash && existing.source_package == operation.source_package
            {
                statuses.push(serde_json::json!({
                    "skill":skill,
                    "status":"already_adopted",
                    "canonical_path":a.library.join(&operation.source_package),
                }));
                continue;
            }
            return Err(anyhow::anyhow!(
                "package already has a managed relationship"
            ));
        }
        if prepared
            .local_adoptions
            .iter()
            .any(|(existing_key, existing)| {
                existing_key == &format!("local:{skill}")
                    || existing.source_package == operation.source_package
            })
        {
            return Err(anyhow::anyhow!(
                "package already has a managed relationship"
            ));
        }
        if prepared.subscriptions.values().any(|x| x.skill == skill)
            || prepared.publications.values().any(|x| x.skill == skill)
            || prepared.harness_links.values().any(|x| x.skill == skill)
            || prepared
                .harness_sets
                .values()
                .any(|x| x.members.contains(&skill))
        {
            return Err(anyhow::anyhow!(
                "package already has a managed relationship"
            ));
        }
        prepared.local_adoptions.insert(
            key,
            crate::LocalAdoption {
                skill: skill.clone(),
                source_path: a.library.display().to_string(),
                source_package: operation.source_package.clone(),
                content_hash: hash,
                local_path: destination.display().to_string(),
                status: "adopted".into(),
            },
        );
        statuses.push(
            serde_json::json!({"skill":skill,"status":"adopted","canonical_path":destination}),
        );
    }
    let previous = a.state.clone();
    a.state = prepared;
    if let Err(error) = a.save() {
        a.state = previous;
        return Err(error.context("persist onboarding adoption state"));
    }
    if statuses.len() == 1 {
        return Ok(serde_json::json!({
            "skill": statuses[0]["skill"],
            "status": statuses[0]["status"],
            "canonical_path": statuses[0]["canonical_path"]        }));
    }
    Ok(serde_json::json!({"version":2,"operations":statuses}))
}

fn root_status(path: &Path) -> &'static str {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "unreadable",
        Ok(m) if m.file_type().is_symlink() => "unsupported_reparse",
        Ok(m) if !m.is_dir() => "unsupported_special",
        Ok(_) => "present",
    }
}
fn safe_canonical(path: &Path) -> anyhow::Result<PathBuf> {
    crate::filesystem::assert_no_symlink_path(path, Path::new("."))?;
    crate::filesystem::reject_reparse_point(path, "persisted path")?;
    Ok(fs::canonicalize(path)?)
}
fn explicit_root(path: &Path) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(anyhow::anyhow!(
            "explicit discovery root must be absolute: {}",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        anyhow::anyhow!(
            "explicit discovery root must be an existing directory: {}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow::anyhow!(
            "explicit discovery root must be an existing directory: {}",
            path.display()
        ));
    }
    let canonical = safe_canonical(path)?;
    Ok(canonical)
}
fn package_name(path: &Path) -> Result<Option<String>> {
    let Ok(meta) = fs::symlink_metadata(path.join("SKILL.md")) else {
        return Ok(None);
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Ok(None);
    }
    Ok(Some(crate::filesystem::manifest_name(path)?))
}
fn diagnostic(code: &'static str, path: &Path, detail: &'static str) -> DiagnosticReport {
    DiagnosticReport {
        code,
        path: path.display().to_string(),
        detail,
    }
}

fn normalized_relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    let normalized = relative.replace('\\', "/");
    if normalized.is_empty() {
        ".".into()
    } else {
        normalized
    }
}

fn is_in_changed_subtree(path: &str, subtree: &str) -> bool {
    subtree == "." || path == subtree || path.starts_with(&(subtree.to_owned() + "/"))
}

fn discard_changed_subtree(
    root: &Path,
    current: &Path,
    occurrence_root: Option<&str>,
    packages: &mut Vec<PackageReport>,
    occurrences: &mut Vec<OccurrenceReport>,
) {
    let subtree = normalized_relative_path(root, current);
    if occurrence_root.is_none() {
        packages.retain(|package| !is_in_changed_subtree(&package.path, &subtree));
    } else {
        let root_name = occurrence_root.unwrap_or_default();
        occurrences.retain(|occurrence| {
            occurrence.root != root_name || !is_in_changed_subtree(&occurrence.path, &subtree)
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    root: &Path,
    current: &Path,
    managed: &BTreeSet<PathBuf>,
    packages: &mut Vec<PackageReport>,
    occurrences: &mut Vec<OccurrenceReport>,
    diagnostics: &mut Vec<DiagnosticReport>,
    visited: &mut BTreeSet<PathBuf>,
    occurrence_root: Option<&str>,
) {
    let Ok(before) = crate::recovery::directory_identity(current) else {
        diagnostics.push(diagnostic(
            "unsafe_record",
            current,
            "directory identity could not be captured",
        ));
        return;
    };
    if crate::filesystem::assert_no_symlink_path(current, Path::new(".")).is_err() {
        diagnostics.push(diagnostic(
            "unsupported_reparse",
            current,
            "directory or ancestor is unsafe",
        ));
        return;
    }
    let Ok(real) = fs::canonicalize(current) else {
        diagnostics.push(diagnostic(
            "unreadable_directory",
            current,
            "directory could not be canonicalized",
        ));
        return;
    };
    if !visited.insert(real.clone()) {
        diagnostics.push(diagnostic(
            "cycle_or_duplicate",
            current,
            "directory identity already visited",
        ));
        return;
    }
    match package_name(current) {
        Ok(Some(name)) => {
            let rel = normalized_relative_path(root, current);
            if let Some(hroot) = occurrence_root {
                occurrences.push(OccurrenceReport {
                    name,
                    root: hroot.into(),
                    path: rel,
                    status: if managed.contains(&real) {
                        "managed"
                    } else {
                        "unmanaged"
                    }
                    .into(),
                });
            } else {
                packages.push(PackageReport {
                    name,
                    path: rel,
                    status: if managed.contains(&real) {
                        "managed"
                    } else {
                        "unmanaged"
                    }
                    .into(),
                });
            }
        }
        Ok(None) => {}
        Err(_) => diagnostics.push(diagnostic(
            "invalid_manifest",
            &current.join("SKILL.md"),
            "SKILL.md manifest is invalid",
        )),
    }
    let Ok(entries) = fs::read_dir(current) else {
        diagnostics.push(diagnostic(
            "unreadable_directory",
            current,
            "directory could not be read",
        ));
        return;
    };
    let mut children: Vec<_> = entries.filter_map(Result::ok).collect();
    children.sort_by_key(|e| e.file_name());
    for entry in children {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            diagnostics.push(diagnostic(
                "unreadable_entry",
                &path,
                "metadata could not be read",
            ));
            continue;
        };
        if meta.file_type().is_symlink() {
            diagnostics.push(diagnostic(
                "unsupported_reparse",
                &path,
                "symlink or reparse point was not followed",
            ));
            continue;
        }
        if meta.is_dir() {
            walk(
                root,
                &path,
                managed,
                packages,
                occurrences,
                diagnostics,
                visited,
                occurrence_root,
            );
        } else if !meta.is_file() {
            diagnostics.push(diagnostic(
                "unsupported_special",
                &path,
                "special filesystem entry was not inspected",
            ));
        }
    }
    if crate::recovery::directory_identity(current).ok() != Some(before) {
        discard_changed_subtree(root, current, occurrence_root, packages, occurrences);
        diagnostics.push(diagnostic(
            "directory_changed",
            current,
            "directory identity changed during traversal; subtree discarded",
        ));
    }
}

#[allow(dead_code)]
pub(crate) fn discover(library: &Path, state: &crate::State) -> Result<DiscoverReport> {
    discover_with_roots(library, state, &[])
}

pub(crate) fn discover_with_roots(
    library: &Path,
    state: &crate::State,
    explicit: &[PathBuf],
) -> Result<DiscoverReport> {
    let mut roots = vec![RootReport {
        kind: "canonical_library".into(),
        path: library.display().to_string(),
        status: root_status(library).into(),
    }];
    let mut diagnostics = Vec::new();
    let mut managed = BTreeSet::new();
    let mut harnesses = BTreeSet::new();
    for v in state.harness_links.values() {
        harnesses.insert(v.harness_root.clone());
        match safe_canonical(Path::new(&v.canonical_path)) {
            Ok(p) => {
                managed.insert(p);
            }
            Err(_) => diagnostics.push(diagnostic(
                "invalid_persisted_path",
                Path::new(&v.canonical_path),
                "persisted path is unsafe or cannot be canonicalized",
            )),
        }
    }
    for v in state.harness_sets.values() {
        harnesses.insert(v.harness_root.clone());
    }
    for v in state.local_adoptions.values() {
        match safe_canonical(Path::new(&v.local_path)) {
            Ok(p) => {
                managed.insert(p);
            }
            Err(_) => diagnostics.push(diagnostic(
                "invalid_persisted_path",
                Path::new(&v.local_path),
                "persisted path is unsafe or cannot be canonicalized",
            )),
        }
    }
    for v in state.subscriptions.values() {
        match safe_canonical(Path::new(&v.local_path)) {
            Ok(p) => {
                managed.insert(p);
            }
            Err(_) => diagnostics.push(diagnostic(
                "invalid_persisted_path",
                Path::new(&v.local_path),
                "persisted path is unsafe or cannot be canonicalized",
            )),
        }
    }
    for raw in harnesses {
        let path = Path::new(&raw);
        let status = root_status(path);
        roots.push(RootReport {
            kind: "configured_harness".into(),
            path: raw.clone(),
            status: status.into(),
        });
        if status != "present" {
            diagnostics.push(diagnostic(
                "known_root_unavailable",
                path,
                "persisted root was not scanned",
            ));
        }
    }
    let mut explicit_roots = BTreeSet::new();
    for raw in explicit {
        explicit_roots.insert(explicit_root(raw)?);
    }
    for root in &explicit_roots {
        roots.push(RootReport {
            kind: "explicit_root".into(),
            path: root.display().to_string(),
            status: "present".into(),
        });
    }
    let mut packages = Vec::new();
    let mut occurrences = Vec::new();
    if root_status(library) == "present" {
        walk(
            library,
            library,
            &managed,
            &mut packages,
            &mut occurrences,
            &mut diagnostics,
            &mut BTreeSet::new(),
            None,
        );
    } else {
        diagnostics.push(diagnostic(
            "library_unavailable",
            library,
            "configured library was not scanned",
        ));
    }
    // Harness traversal above uses temporary vectors only to keep library reports separate.
    // Rewalk into the actual occurrence vector after deterministic root validation.
    for root in roots
        .iter()
        .filter(|r| r.kind == "configured_harness" && r.status == "present")
    {
        let p = Path::new(&root.path);
        walk(
            p,
            p,
            &managed,
            &mut Vec::new(),
            &mut occurrences,
            &mut diagnostics,
            &mut BTreeSet::new(),
            Some(&root.path),
        );
    }
    for root in &explicit_roots {
        let id = root.display().to_string();
        walk(
            root,
            root,
            &managed,
            &mut Vec::new(),
            &mut occurrences,
            &mut diagnostics,
            &mut BTreeSet::new(),
            Some(&id),
        );
    }
    roots.sort_by(|a, b| (&a.kind, &a.path).cmp(&(&b.kind, &b.path)));
    packages.sort_by(|a, b| (&a.path, &a.name).cmp(&(&b.path, &b.name)));
    occurrences.sort_by(|a, b| (&a.root, &a.path, &a.name).cmp(&(&b.root, &b.path, &b.name)));
    diagnostics.sort_by(|a, b| (&a.code, &a.path, &a.detail).cmp(&(&b.code, &b.path, &b.detail)));
    let proposals = packages
        .iter()
        .filter(|package| package.status == "unmanaged")
        .map(|package| ProposalReport {
            kind: "adopt",
            path: package.path.clone(),
            requires_approval: true,
            destructive: false,
        })
        .collect();
    Ok(DiscoverReport {
        schema_version: 1,
        mode: "read_only",
        roots,
        packages,
        occurrences,
        proposals,
        diagnostics,
        capabilities: BTreeMap::from([
            ("harness_filtering", "unsupported"),
            ("harness_reload", "unsupported"),
            ("autonomous_curation", "unsupported"),
        ]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    #[test]
    fn changed_subtree_discard_respects_path_component_boundaries() {
        let root = Path::new("/library");
        let current = root.join("a");
        let mut packages = vec![
            PackageReport {
                name: "descendant".into(),
                path: "a/child".into(),
                status: "unmanaged".into(),
            },
            PackageReport {
                name: "sibling".into(),
                path: "a2/child".into(),
                status: "unmanaged".into(),
            },
        ];
        let mut occurrences = Vec::new();
        discard_changed_subtree(root, &current, None, &mut packages, &mut occurrences);
        assert_eq!(
            packages.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
            ["a2/child"]
        );

        let mut packages = Vec::new();
        let mut occurrences = vec![
            OccurrenceReport {
                name: "descendant".into(),
                root: "harness".into(),
                path: "a/child".into(),
                status: "unmanaged".into(),
            },
            OccurrenceReport {
                name: "sibling".into(),
                root: "harness".into(),
                path: "a2/child".into(),
                status: "unmanaged".into(),
            },
        ];
        discard_changed_subtree(
            root,
            &current,
            Some("harness"),
            &mut packages,
            &mut occurrences,
        );
        assert_eq!(
            occurrences
                .iter()
                .map(|o| o.path.as_str())
                .collect::<Vec<_>>(),
            ["a2/child"]
        );
    }

    #[test]
    fn discovery_report_is_versioned_and_read_only() {
        let d = tempdir().unwrap();
        fs::write(d.path().join("SKILL.md"), "name: root\n").unwrap();
        fs::create_dir(d.path().join("pkg")).unwrap();
        fs::write(d.path().join("pkg/SKILL.md"), "name: pkg\n").unwrap();
        let n = fs::read_dir(d.path()).unwrap().count();
        let r = discover(d.path(), &crate::State::default()).unwrap();
        assert_eq!(r.schema_version, 1);
        assert_eq!(r.packages.len(), 2);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), n);
    }
}
