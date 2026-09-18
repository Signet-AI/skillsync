use anyhow::Result;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

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
fn package_name(path: &Path) -> Option<String> {
    let meta = fs::symlink_metadata(path.join("SKILL.md")).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return None;
    }
    path.file_name().map(|n| n.to_string_lossy().into_owned())
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
    if let Some(name) = package_name(current) {
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

pub(crate) fn discover(library: &Path, state: &crate::State) -> Result<DiscoverReport> {
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
    roots.sort_by(|a, b| (&a.kind, &a.path).cmp(&(&b.kind, &b.path)));
    packages.sort_by(|a, b| (&a.path, &a.name).cmp(&(&b.path, &b.name)));
    occurrences.sort_by(|a, b| (&a.root, &a.path, &a.name).cmp(&(&b.root, &b.path, &b.name)));
    diagnostics.sort_by(|a, b| (&a.code, &a.path, &a.detail).cmp(&(&b.code, &b.path, &b.detail)));
    Ok(DiscoverReport {
        schema_version: 1,
        mode: "read_only",
        roots,
        packages,
        occurrences,
        proposals: Vec::new(),
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
        fs::write(d.path().join("SKILL.md"), "x").unwrap();
        fs::create_dir(d.path().join("pkg")).unwrap();
        fs::write(d.path().join("pkg/SKILL.md"), "x").unwrap();
        let n = fs::read_dir(d.path()).unwrap().count();
        let r = discover(d.path(), &crate::State::default()).unwrap();
        assert_eq!(r.schema_version, 1);
        assert_eq!(r.packages.len(), 2);
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), n);
    }
}
