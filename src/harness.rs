use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use crate::filesystem::open_directory_fd;
use crate::filesystem::{
    assert_no_symlink_path, checked_regular_path, manifest_name, strict_component,
};
use crate::recovery::{directory_identity, DirectoryIdentity};
use crate::{set_member_name, App, HarnessLink, HarnessSetEnablement, State};

fn harness_key(skill: &str, root: &Path) -> String {
    let mut h = Sha256::new();
    h.update(b"harness-link\0");
    h.update(skill.as_bytes());
    h.update([0]);
    h.update(root.to_string_lossy().as_bytes());
    format!("hlink-{:x}", h.finalize())
}

fn validate_harness_link_record(key: &str, record: &HarnessLink, library: &Path) -> Result<()> {
    let skill = strict_component(&record.skill, "harness link skill")?;
    let root = PathBuf::from(&record.harness_root);
    if !root.is_absolute() {
        return Err(anyhow!("harness link root must be absolute"));
    }
    assert_no_symlink_path(&root, Path::new("."))?;
    if root.exists() {
        let metadata = fs::symlink_metadata(&root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(anyhow!("harness link root is not a regular directory"));
        }
        if fs::canonicalize(&root)? != root {
            return Err(anyhow!("harness link root is not canonical"));
        }
    }
    let canonical = library.join(&skill);
    assert_no_symlink_path(&canonical, Path::new("."))?;
    let link = root.join(&skill);
    if Path::new(&record.canonical_path) != canonical.as_path() {
        return Err(anyhow!(
            "harness link canonical path does not match library"
        ));
    }
    if Path::new(&record.link_path) != link.as_path() {
        return Err(anyhow!("harness link path does not match harness root"));
    }
    if key != harness_key(&skill, &root) {
        return Err(anyhow!(
            "harness link relationship key does not match record"
        ));
    }
    if record.status != "linked" {
        return Err(anyhow!(
            "unsupported harness link status: {}",
            record.status
        ));
    }
    Ok(())
}

pub(crate) fn validate_harness_links(state: &State, library: &Path) -> Result<()> {
    for (key, record) in &state.harness_links {
        validate_harness_link_record(key, record, library)?;
    }
    Ok(())
}

pub(crate) fn validate_harness_sets(state: &State, library: &Path) -> Result<()> {
    for (relationship, record) in &state.harness_sets {
        let set = strict_component(&record.set, "harness set name")?;
        state
            .sets
            .get(&set)
            .ok_or_else(|| anyhow!("harness set refers to missing set: {set}"))?;
        let root = persisted_harness_root(Path::new(&record.harness_root))?;
        if Path::new(&record.harness_root) != root {
            return Err(anyhow!("harness set root is not canonical"));
        }
        if harness_set_key(&set, &root) != *relationship {
            return Err(anyhow!(
                "harness set relationship key does not match record"
            ));
        }
        // Enablement is a one-time expansion. Validate the recorded member
        // identities and their owned links, but do not compare them with the
        // set's current membership: later set changes must not strand links.
        for member in &record.members {
            let skill = set_member_name(member)?;
            let key = harness_key(&skill, &root);
            let link = state
                .harness_links
                .get(&key)
                .ok_or_else(|| anyhow!("harness set link record missing"))?;
            validate_harness_link_record(&key, link, library)?;
            if Path::new(&link.harness_root) != root
                || Path::new(&link.link_path) != root.join(&skill)
                || link.status != "linked"
            {
                return Err(anyhow!("harness set link record does not match set"));
            }
        }
    }
    Ok(())
}

fn resolve_link_target(link: &Path) -> Result<PathBuf> {
    let target = fs::read_link(link)?;
    if target.is_absolute() {
        Ok(target)
    } else {
        Ok(link
            .parent()
            .ok_or_else(|| anyhow!("harness link has no parent"))?
            .join(target))
    }
}

fn link_targets_match(target: &Path, expected: &Path) -> Result<bool> {
    if target == expected {
        return Ok(true);
    }
    match (fs::canonicalize(target), fs::canonicalize(expected)) {
        (Ok(actual), Ok(expected)) => Ok(actual == expected),
        _ => Ok(false),
    }
}

fn create_directory_link(target: &Path, link: &Path) -> Result<()> {
    if std::env::var("SKILLSYNC_TEST_FAIL_CREATE_MEMBER").as_deref()
        == Ok(link
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""))
    {
        return Err(anyhow!(
            "injected directory-link creation failure (test-only)"
        ));
    }
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let parent = link
            .parent()
            .ok_or_else(|| anyhow!("harness link has no parent"))?;
        let name = link
            .file_name()
            .ok_or_else(|| anyhow!("harness link has no name"))?;
        let parent_fd = open_directory_fd(parent)?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        let name = CString::new(name.as_bytes())?;
        let result = unsafe { libc::symlinkat(target.as_ptr(), parent_fd, name.as_ptr()) };
        let error = if result == 0 {
            None
        } else {
            Some(std::io::Error::last_os_error())
        };
        unsafe { libc::close(parent_fd) };
        error.map_or(Ok(()), Err).map_err(Into::into)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
            .with_context(|| "create directory symlink (Windows privilege may be required)")?;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(anyhow!("directory links are unsupported on this platform"))
    }
}

fn remove_directory_link(link: &Path) -> Result<()> {
    if std::env::var("SKILLSYNC_TEST_FAIL_REMOVE_MEMBER").as_deref()
        == Ok(link.file_name().and_then(|n| n.to_str()).unwrap_or(""))
    {
        return Err(anyhow!(
            "injected directory-link removal failure (test-only)"
        ));
    }
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let parent = link
            .parent()
            .ok_or_else(|| anyhow!("harness link has no parent"))?;
        let name = link
            .file_name()
            .ok_or_else(|| anyhow!("harness link has no name"))?;
        let parent_fd = open_directory_fd(parent)?;
        let name = CString::new(name.as_bytes())?;
        let result = unsafe { libc::unlinkat(parent_fd, name.as_ptr(), 0) };
        let error = if result == 0 {
            None
        } else {
            Some(std::io::Error::last_os_error())
        };
        unsafe { libc::close(parent_fd) };
        error.map_or(Ok(()), Err).map_err(Into::into)
    }
    #[cfg(windows)]
    {
        let metadata = fs::symlink_metadata(link)?;
        if !metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "harness path is not a directory link: {}",
                link.display()
            ));
        }
        fs::remove_dir(link)?;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(anyhow!("directory links are unsupported on this platform"))
    }
}

fn ensure_link_absent(link: &Path) -> Result<()> {
    match fs::symlink_metadata(link) {
        Ok(_) => Err(anyhow!("harness link still exists: {}", link.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn harness_link_health(a: &App) -> Result<Vec<serde_json::Value>> {
    let mut health = Vec::new();
    for (key, record) in &a.state.harness_links {
        let link = PathBuf::from(&record.link_path);
        let expected = PathBuf::from(&record.canonical_path);
        let (status, message) = if !Path::new(&record.harness_root).exists() {
            (
                "missing_root",
                Some("harness root is unavailable; relationship is degraded".to_owned()),
            )
        } else {
            match fs::symlink_metadata(&link) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                    "missing",
                    Some("recorded harness link is missing".to_owned()),
                ),
                Err(error) => ("unreadable", Some(error.to_string())),
                Ok(metadata) if !metadata.file_type().is_symlink() => (
                    "collision",
                    Some("recorded harness path is not a symlink".to_owned()),
                ),
                Ok(_) => match resolve_link_target(&link)
                    .and_then(|target| link_targets_match(&target, &expected))
                {
                    Ok(true) => ("healthy", None),
                    Ok(false) => (
                        "wrong_target",
                        Some("link target does not match".to_owned()),
                    ),
                    Err(error) => ("unreadable", Some(error.to_string())),
                },
            }
        };
        health.push(serde_json::json!({
            "relationship": key,
            "skill": record.skill,
            "harness_root": record.harness_root,
            "link_path": record.link_path,
            "status": status,
            "message": message,
        }));
    }
    for (key, record) in &a.state.harness_sets {
        if record.members.is_empty() {
            let missing_root = !Path::new(&record.harness_root).exists();
            health.push(serde_json::json!({
                "relationship": key,
                "set": record.set,
                "harness_root": record.harness_root,
                "status": if missing_root { "missing_root" } else { "healthy" },
                "message": if missing_root {
                    Some("harness root is unavailable; relationship is degraded")
                } else {
                    None::<&str>
                },
            }));
        }
    }
    Ok(health)
}

fn existing_harness_root(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        return Err(anyhow!(
            "harness root must be an absolute existing directory"
        ));
    }
    assert_no_symlink_path(root, Path::new("."))?;
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| anyhow!("harness root does not exist: {}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "harness root must be a regular directory: {}",
            root.display()
        ));
    }
    Ok(fs::canonicalize(root)?)
}

fn persisted_harness_root(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        return Err(anyhow!("harness root must be absolute"));
    }
    assert_no_symlink_path(root, Path::new("."))?;
    if root.exists() {
        existing_harness_root(root)
    } else {
        Ok(root.to_path_buf())
    }
}

pub(crate) fn inventory_harness_health(
    state: &State,
    _library: &Path,
) -> Result<Vec<serde_json::Value>> {
    let mut result = Vec::new();
    for (key, record) in &state.harness_sets {
        let root = Path::new(&record.harness_root);
        let (status, message) = if !root.exists() {
            (
                "missing_root",
                Some("harness root is unavailable; relationship is degraded".to_owned()),
            )
        } else if !root.is_dir() {
            (
                "unreadable",
                Some("harness root is not a directory".to_owned()),
            )
        } else {
            let mut status = "healthy";
            let mut message = None;
            for member in &record.members {
                let Ok(skill) = set_member_name(member) else {
                    status = "unreadable";
                    message = Some("invalid set member".into());
                    break;
                };
                let link_key = harness_key(&skill, root);
                let Some(link) = state.harness_links.get(&link_key) else {
                    status = "missing";
                    message = Some("recorded harness member link is missing".into());
                    break;
                };
                if link.status != "linked" {
                    status = "unreadable";
                    message = Some("recorded harness member link is invalid".into());
                    break;
                }
                let path = Path::new(&link.link_path);
                match fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        status = "missing";
                        message = Some("recorded harness member link is missing".into());
                        break;
                    }
                    Err(error) => {
                        status = "unreadable";
                        message = Some(error.to_string());
                        break;
                    }
                    Ok(metadata) if !metadata.file_type().is_symlink() => {
                        status = "collision";
                        message = Some("recorded harness path is not a symlink".into());
                        break;
                    }
                    Ok(_) => match resolve_link_target(path).and_then(|target| {
                        link_targets_match(&target, Path::new(&link.canonical_path))
                    }) {
                        Ok(true) => {}
                        Ok(false) => {
                            status = "wrong_target";
                            message = Some("link target does not match".into());
                            break;
                        }
                        Err(error) => {
                            status = "unreadable";
                            message = Some(error.to_string());
                            break;
                        }
                    },
                }
            }
            (status, message)
        };
        result.push(serde_json::json!({"relationship": key, "set": record.set, "harness_root": record.harness_root, "status": status, "message": message}));
    }
    Ok(result)
}
pub(crate) fn list(a: &App) -> Result<serde_json::Value> {
    Ok(serde_json::json!({"links": a.state.harness_links}))
}
pub(crate) fn harness_link(a: &mut App, root: &Path, raw_skill: &str) -> Result<serde_json::Value> {
    let skill = strict_component(raw_skill, "skill name")?;
    let root = existing_harness_root(root)?;
    let canonical = a.library.join(&skill);
    assert_no_symlink_path(&a.library, Path::new(&skill))?;
    if !canonical.is_dir() || !checked_regular_path(&canonical.join("SKILL.md"), "skill manifest")?
    {
        return Err(anyhow!("skill not found in library: {skill}"));
    }
    let link = root.join(&skill);
    assert_no_symlink_path(&root, Path::new("."))?;
    if fs::symlink_metadata(&link).is_ok() {
        return Err(anyhow!(
            "harness skill path already exists: {}",
            link.display()
        ));
    }
    let key = harness_key(&skill, &root);
    if a.state.harness_links.contains_key(&key) {
        return Err(anyhow!("harness link already recorded: {key}"));
    }
    create_directory_link(&canonical, &link)
        .with_context(|| format!("create directory symlink: {}", link.display()))?;
    let record = HarnessLink {
        skill: skill.clone(),
        harness_root: root.display().to_string(),
        canonical_path: canonical.display().to_string(),
        link_path: link.display().to_string(),
        status: "linked".into(),
    };
    a.state.harness_links.insert(key.clone(), record.clone());
    if let Err(error) = a.save() {
        a.state.harness_links.remove(&key);
        return match remove_directory_link(&link).and_then(|_| ensure_link_absent(&link)) {
            Ok(()) => Err(error).context("save harness link state; link rolled back"),
            Err(rollback_error) => Err(anyhow!(
                "save harness link state failed: {error}; link rollback failed: {rollback_error}"
            )),
        };
    }
    Ok(serde_json::json!({"status":"linked","relationship":key,"link":record}))
}

pub(crate) fn harness_unlink(
    a: &mut App,
    root: &Path,
    raw_skill: &str,
) -> Result<serde_json::Value> {
    let skill = strict_component(raw_skill, "skill name")?;
    let root = existing_harness_root(root)?;
    let key = harness_key(&skill, &root);
    let record = a
        .state
        .harness_links
        .get(&key)
        .cloned()
        .ok_or_else(|| anyhow!("harness link not found"))?;
    let relationship = harness_key(&skill, &root);
    validate_harness_link_record(&relationship, &record, &a.library)?;
    let link = root.join(&skill);
    let expected = a.library.join(&skill);
    let target = resolve_link_target(&link).context("recorded harness path is not a symlink")?;
    if !link_targets_match(&target, &expected)? {
        return Err(anyhow!(
            "harness link target does not match canonical skill"
        ));
    }
    remove_directory_link(&link).context("remove harness directory link")?;
    ensure_link_absent(&link)?;
    a.state.harness_links.remove(&key);
    if let Err(error) = a.save() {
        a.state.harness_links.insert(key.clone(), record);
        return match create_directory_link(&expected, &link) {
            Ok(()) => Err(error).context("save harness unlink state; link restored"),
            Err(restore_error) => Err(anyhow!(
                "save harness unlink state failed: {error}; link restore failed: {restore_error}"
            )),
        };
    }
    Ok(serde_json::json!({"status":"unlinked","relationship":key,"canonical_retained":true}))
}

pub(crate) fn harness_set_key(set: &str, root: &Path) -> String {
    let mut h = Sha256::new();
    h.update(b"harness-set\\0");
    h.update(set.as_bytes());
    h.update([0]);
    h.update(root.to_string_lossy().as_bytes());
    format!("hset-{:x}", h.finalize())
}

fn rollback_created_links(created: &[(String, PathBuf, PathBuf)]) -> Result<()> {
    let mut failures = Vec::new();
    for (_, canonical, link) in created.iter().rev() {
        let valid = fs::symlink_metadata(link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
            && resolve_link_target(link)
                .and_then(|target| link_targets_match(&target, canonical))
                .unwrap_or(false);
        if !valid {
            failures.push(format!(
                "{} is no longer the expected native link",
                link.display()
            ));
            continue;
        }
        if let Err(error) = remove_directory_link(link).and_then(|_| ensure_link_absent(link)) {
            failures.push(format!("{}: {error}", link.display()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "rollback failed; recovery required: {}",
            failures.join("; ")
        ))
    }
}

#[derive(Clone)]
struct HarnessSetFsSnapshot {
    library: DirectoryIdentity,
    root: DirectoryIdentity,
    parent: DirectoryIdentity,
    members: BTreeMap<String, DirectoryIdentity>,
}

fn harness_set_fs_snapshot(
    library: &Path,
    root: &Path,
    plans: &[(String, PathBuf, PathBuf)],
) -> Result<HarnessSetFsSnapshot> {
    let parent = root
        .parent()
        .ok_or_else(|| anyhow!("harness root has no parent"))?;
    let mut members = BTreeMap::new();
    for (skill, canonical, _) in plans {
        members.insert(skill.clone(), directory_identity(canonical)?);
    }
    Ok(HarnessSetFsSnapshot {
        library: directory_identity(library)?,
        root: directory_identity(root)?,
        parent: directory_identity(parent)?,
        members,
    })
}

fn verify_harness_set_fs(
    snapshot: &HarnessSetFsSnapshot,
    library: &Path,
    root: &Path,
    skill: &str,
    canonical: &Path,
) -> Result<()> {
    let parent = root
        .parent()
        .ok_or_else(|| anyhow!("harness root has no parent"))?;
    let member = snapshot
        .members
        .get(skill)
        .copied()
        .ok_or_else(|| anyhow!("member identity missing"))?;
    if directory_identity(library)? != snapshot.library
        || directory_identity(root)? != snapshot.root
        || directory_identity(parent)? != snapshot.parent
        || directory_identity(canonical)? != member
    {
        return Err(anyhow!("harness set filesystem identity changed; retry"));
    }
    Ok(())
}

fn verify_harness_set_link(link: &Path, canonical: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(link)?;
    if !metadata.file_type().is_symlink() {
        return Err(anyhow!("harness set link is no longer a native link"));
    }
    let target = resolve_link_target(link)?;
    if !link_targets_match(&target, canonical)? {
        return Err(anyhow!("harness set link target changed"));
    }
    Ok(())
}

pub(crate) fn harness_set(
    a: &mut App,
    root: &Path,
    raw_set: &str,
    enabling: bool,
) -> Result<serde_json::Value> {
    let set = strict_component(raw_set, "set name")?;
    let root = if enabling {
        existing_harness_root(root)?
    } else {
        persisted_harness_root(root)?
    };
    let definition = a
        .state
        .sets
        .get(&set)
        .cloned()
        .ok_or_else(|| anyhow!("set not found: {set}"))?;
    let members = definition
        .members
        .iter()
        .map(|id| set_member_name(id))
        .collect::<Result<Vec<_>>>()?;
    let mut plans = Vec::new();
    for skill in members {
        let canonical = a.library.join(&skill);
        assert_no_symlink_path(&a.library, Path::new(&skill))?;
        if !canonical.is_dir()
            || fs::canonicalize(&canonical)? != canonical
            || manifest_name(&canonical)? != skill
        {
            return Err(anyhow!(
                "set member is not a canonical library package: {skill}"
            ));
        }
        let link = root.join(&skill);
        assert_no_symlink_path(&root, Path::new("."))?;
        plans.push((skill, canonical, link));
    }
    let relationship = harness_set_key(&set, &root);
    if enabling {
        if let Some(record) = a.state.harness_sets.get(&relationship).cloned() {
            // An existing relationship is only idempotent when its persisted
            // enablement and every expanded member link still describe the
            // same safe, live integration. Never repair or mutate a damaged
            // relationship on the idempotence path.
            if record.set != set {
                return Err(anyhow!("harness set record name does not match set"));
            }
            if Path::new(&record.harness_root) != root.as_path() {
                return Err(anyhow!("harness set root does not match relationship"));
            }
            for member in &record.members {
                let skill = set_member_name(member)?;
                let key = harness_key(&skill, &root);
                let link = a
                    .state
                    .harness_links
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| anyhow!("set link record missing"))?;
                validate_harness_link_record(&key, &link, &a.library)?;
                let target = resolve_link_target(Path::new(&link.link_path))
                    .context("recorded set harness path is not a symlink")?;
                if !link_targets_match(&target, Path::new(&link.canonical_path))? {
                    return Err(anyhow!("set link target does not match canonical skill"));
                }
            }
            return Ok(
                serde_json::json!({"status":"already_enabled","set":set,"relationship":relationship}),
            );
        }
        // Preflight every destination before creating any link, so a later
        // collision cannot leave an earlier member partially enabled.
        for (skill, _, link) in &plans {
            // A new set expansion must never adopt an existing path or link
            // record. Only a link created by this expansion can be owned by
            // the resulting set relationship; an existing relationship takes
            // the separate idempotent path above.
            let key = harness_key(skill, &root);
            if a.state.harness_links.contains_key(&key) {
                return Err(anyhow!("harness link already recorded: {key}"));
            }
            if fs::symlink_metadata(link).is_ok() {
                return Err(anyhow!(
                    "harness skill path already exists: {}",
                    link.display()
                ));
            }
        }
        let snapshot = harness_set_fs_snapshot(&a.library, &root, &plans)?;
        let mut created = Vec::new();
        for (skill, canonical, link) in &plans {
            if let Err(error) =
                verify_harness_set_fs(&snapshot, &a.library, &root, skill, canonical)
                    .and_then(|_| ensure_link_absent(link))
            {
                let rollback = rollback_created_links(&created);
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(anyhow!("{error}; {rollback_error}")),
                };
            }
            if let Err(error) = create_directory_link(canonical, link) {
                let rollback = rollback_created_links(&created);
                return match rollback {
                    Ok(()) => Err(error).context("create harness set link; links rolled back"),
                    Err(rollback_error) => Err(anyhow!(
                        "create harness set link failed: {error}; {rollback_error}"
                    )),
                };
            }
            created.push((skill.clone(), canonical.clone(), link.clone()));
            if let Err(error) =
                verify_harness_set_fs(&snapshot, &a.library, &root, skill, canonical)
                    .and_then(|_| verify_harness_set_link(link, canonical))
            {
                let rollback = rollback_created_links(&created);
                return match rollback {
                    Ok(()) => Err(error).context("verify harness set link; links rolled back"),
                    Err(rollback_error) => Err(anyhow!(
                        "verify harness set link failed: {error}; {rollback_error}"
                    )),
                };
            }
        }
        let previous = a.state.clone();
        for (skill, _, _) in &created {
            let key = harness_key(skill, &root);
            a.state.harness_links.insert(
                key,
                HarnessLink {
                    skill: skill.clone(),
                    harness_root: root.display().to_string(),
                    canonical_path: a.library.join(skill).display().to_string(),
                    link_path: root.join(skill).display().to_string(),
                    status: "linked".into(),
                },
            );
        }
        a.state.harness_sets.insert(
            relationship.clone(),
            HarnessSetEnablement {
                set: set.clone(),
                harness_root: root.display().to_string(),
                members: definition.members,
            },
        );
        if let Err(e) = a.save() {
            a.state = previous;
            let rollback = rollback_created_links(&created);
            if let Err(rollback_error) = rollback {
                return Err(anyhow!(
                    "save harness set state failed: {e}; {rollback_error}"
                ));
            }
            return Err(e).context("save harness set state; links rolled back");
        }
        Ok(
            serde_json::json!({"status":"enabled","set":set,"relationship":relationship,"members":plans.iter().map(|x|x.0.clone()).collect::<Vec<_>>()}),
        )
    } else {
        let record = a
            .state
            .harness_sets
            .get(&relationship)
            .cloned()
            .ok_or_else(|| anyhow!("set enablement not found: {set}"))?;
        let previous = a.state.clone();
        // Validate every member before unlinking any member.
        let mut removal_plan = Vec::new();
        for skill in &record.members {
            let key = harness_key(&set_member_name(skill)?, &root);
            let link = a
                .state
                .harness_links
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow!("set link record missing"))?;
            validate_harness_link_record(&key, &link, &a.library)?;
            let target = resolve_link_target(Path::new(&link.link_path))?;
            if !link_targets_match(&target, Path::new(&link.canonical_path))? {
                return Err(anyhow!("set link target does not match canonical skill"));
            }
            removal_plan.push((key, link));
        }
        let removal_snapshot_plans = removal_plan
            .iter()
            .map(|(_, link)| {
                (
                    link.skill.clone(),
                    PathBuf::from(&link.canonical_path),
                    PathBuf::from(&link.link_path),
                )
            })
            .collect::<Vec<_>>();
        let snapshot = harness_set_fs_snapshot(&a.library, &root, &removal_snapshot_plans)?;
        let mut removed: Vec<(String, HarnessLink)> = Vec::new();
        for (key, link) in removal_plan {
            let skill = link.skill.clone();
            let canonical = Path::new(&link.canonical_path);
            if let Err(error) =
                verify_harness_set_fs(&snapshot, &a.library, &root, &skill, canonical)
                    .and_then(|_| verify_harness_set_link(Path::new(&link.link_path), canonical))
                    .and_then(|_| remove_directory_link(Path::new(&link.link_path)))
                    .and_then(|_| {
                        verify_harness_set_fs(&snapshot, &a.library, &root, &skill, canonical)
                    })
                    .and_then(|_| ensure_link_absent(Path::new(&link.link_path)))
            {
                let mut failures = Vec::new();
                for (_, prior) in removed.iter().rev() {
                    let link_path = Path::new(&prior.link_path);
                    if let Err(e) =
                        create_directory_link(Path::new(&prior.canonical_path), link_path).and_then(
                            |_| {
                                let target = resolve_link_target(link_path)?;
                                if link_targets_match(&target, Path::new(&prior.canonical_path))? {
                                    Ok(())
                                } else {
                                    Err(anyhow!("restored link target mismatch"))
                                }
                            },
                        )
                    {
                        failures.push(format!("{}: {e}", link_path.display()));
                    }
                }
                a.state = previous;
                return if failures.is_empty() {
                    Err(error).context("disable harness set; links restored")
                } else {
                    Err(anyhow!(
                        "{error}; recovery required: {}",
                        failures.join("; ")
                    ))
                };
            }
            removed.push((key, link));
        }
        for (key, _) in &removed {
            a.state.harness_links.remove(key);
        }
        a.state.harness_sets.remove(&relationship);
        if let Err(e) = a.save() {
            a.state = previous;
            let mut failures = Vec::new();
            for (_, link) in removed.iter().rev() {
                if let Err(error) = create_directory_link(
                    Path::new(&link.canonical_path),
                    Path::new(&link.link_path),
                )
                .and_then(|_| {
                    let target = resolve_link_target(Path::new(&link.link_path))?;
                    if link_targets_match(&target, Path::new(&link.canonical_path))? {
                        Ok(())
                    } else {
                        Err(anyhow!("restored link target mismatch"))
                    }
                }) {
                    failures.push(format!("{}: {error}", link.link_path));
                }
            }
            return if failures.is_empty() {
                Err(e).context("save harness set state; links restored")
            } else {
                Err(anyhow!(
                    "save harness set state failed: {e}; recovery required: {}",
                    failures.join("; ")
                ))
            };
        }
        Ok(
            serde_json::json!({"status":"disabled","set":set,"relationship":relationship,"canonical_retained":true}),
        )
    }
}
