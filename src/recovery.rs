use anyhow::{anyhow, Context, Result};
use std::{
    fs,
    path::{Component, Path},
};

use crate::filesystem::{
    assert_no_symlink_path, checked_regular_path, copy_complete_tree, copy_tree, discover,
    hash_dir, install_dir_noreplace, manifest_name, reject_reparse_point, strict_component,
    validate_state_path,
};
use crate::*;

#[cfg(unix)]
pub(crate) type DirectoryIdentity = (u64, u64);
#[cfg(windows)]
pub(crate) type DirectoryIdentity = (u32, u32, u32);

pub(crate) fn directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path).context("read canonical library identity")?;
        if !metadata.is_dir() {
            return Err(anyhow!("canonical library is not a directory"));
        }
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::{iter, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        fn identity(path: &Path) -> Result<(u32, u32, u32)> {
            let wide = path
                .as_os_str()
                .encode_wide()
                .chain(iter::once(0))
                .collect::<Vec<_>>();
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    0x80000000,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
            let ok = unsafe { GetFileInformationByHandle(handle, &mut info) != 0 };
            unsafe { CloseHandle(handle) };
            if !ok {
                return Err(std::io::Error::last_os_error().into());
            }
            if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(anyhow!("canonical library is a reparse point"));
            }
            Ok((
                info.dwVolumeSerialNumber,
                info.nFileIndexHigh,
                info.nFileIndexLow,
            ))
        }

        identity(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(anyhow!(
            "directory identity is unavailable on this platform"
        ))
    }
}

pub(crate) fn remove_owned_directory(
    root: &Path,
    path: &Path,
    identity: Option<DirectoryIdentity>,
    hash: &str,
) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| anyhow!("cleanup path escaped root"))?;
    validate_state_path(root, path, "cleanup")?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    reject_reparse_point(path, "cleanup target")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "cleanup target is not an owned regular directory: {}",
            path.display()
        ));
    }
    let current = directory_identity(path)?;
    if identity != Some(current) || hash_dir(path)? != hash {
        return Err(anyhow!(
            "cleanup ownership changed; recovery required: {}",
            relative.display()
        ));
    }
    fs::remove_dir_all(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(anyhow!(
                "cleanup could not verify removal; replacement is not a regular directory: {}",
                path.display()
            ));
        }
        Ok(_) => {
            return Err(anyhow!(
                "cleanup could not verify removal: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }
    Ok(())
}

pub(crate) fn import_local(
    a: &mut App,
    source: &Path,
    requested: Option<&str>,
) -> Result<serde_json::Value> {
    use std::io::IsTerminal;
    assert_no_symlink_path(source, Path::new("."))?;
    if !source.is_dir() {
        return Err(anyhow!("import source is not a directory"));
    }
    let source = fs::canonicalize(source).context("canonicalize import source")?;
    if !source.is_dir() {
        return Err(anyhow!("import source is not a directory"));
    }
    if source == a.library || source.starts_with(&a.library) || a.library.starts_with(&source) {
        return Err(anyhow!("import source overlaps canonical library"));
    }
    reject_reparse_point(&source, "import source")?;
    let found = discover(&source)?;
    if found.is_empty() {
        return Err(anyhow!("no SKILL.md packages found in import source"));
    }
    let selected = match requested {
        Some(query) => {
            let query = skill_query(query)?;
            found
                .iter()
                .find(|(name, _, rel)| name == &query || rel == &query)
                .ok_or_else(|| anyhow!("skill not found in import source: {query}"))?
        }
        None => {
            if !std::io::stdin().is_terminal() || found.len() != 1 {
                return Err(anyhow!(
                    "--skill is required for noninteractive import or multiple packages"
                ));
            }
            &found[0]
        }
    };
    let (name, package, rel) = selected;
    let name = strict_component(name, "manifest skill name")?;
    let library_identity_before = directory_identity(&a.library)?;
    let source_package_hash = hash_dir(package).context("hash discovered import package")?;
    let destination = a.library.join(&name);
    if !destination.starts_with(&a.library) {
        return Err(anyhow!("destination escaped library"));
    }
    let staging_parent = tempfile::tempdir_in(&a.library)?;
    let staged = staging_parent.path().join("package");
    copy_tree(package, &staged)?;
    let incoming_hash = hash_dir(&staged)?;
    let source_package_hash_after =
        hash_dir(package).context("recheck discovered import package")?;
    if source_package_hash_after != source_package_hash {
        return Err(anyhow!("source package changed during import; retry"));
    }
    let destination_metadata = match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            assert_no_symlink_path(&a.library, Path::new(&name))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(anyhow!(
                    "canonical package destination is not a regular directory"
                ));
            }
            if fs::canonicalize(&destination)? != destination {
                return Err(anyhow!(
                    "canonical package destination is not a canonical directory"
                ));
            }
            checked_regular_path(&destination.join("SKILL.md"), "skill manifest")?;
            Some(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            assert_no_symlink_path(&a.library, Path::new(&name))?;
            None
        }
        Err(error) => return Err(error.into()),
    };
    if destination_metadata.is_some() {
        let existing_hash = hash_dir(&destination)?;
        if existing_hash == incoming_hash {
            let key = format!("local:{name}");
            let previous_state = a.state.clone();
            a.state.local_adoptions.insert(
                key,
                LocalAdoption {
                    skill: name.clone(),
                    source_path: source.display().to_string(),
                    source_package: rel.clone(),
                    content_hash: incoming_hash,
                    local_path: destination.display().to_string(),
                    status: "adopted".into(),
                },
            );
            if let Err(error) = a.save() {
                a.state = previous_state;
                return Err(error).context("persist local adoption state");
            }
            return Ok(
                serde_json::json!({"skill":name,"status":"already_present","provenance":"recorded"}),
            );
        }
        return Err(anyhow!(
            "canonical package already exists with different contents; not overwritten"
        ));
    }
    let key = format!("local:{name}");
    assert_no_symlink_path(&a.library, Path::new("."))?;
    if hash_dir(package)? != source_package_hash {
        return Err(anyhow!("source package changed during import; retry"));
    }
    if directory_identity(&a.library)? != library_identity_before {
        return Err(anyhow!("canonical library changed during import; retry"));
    }
    if std::env::var("SKILLSYNC_TEST_IMPORT_COLLISION").as_deref() == Ok("1") {
        fs::create_dir_all(&destination)?;
        fs::write(destination.join("SKILL.md"), "external collision\n")?;
    }
    install_dir_noreplace(&staged, &destination)
        .context("install canonical package without replacement")?;
    if std::env::var("SKILLSYNC_TEST_IMPORT_VERIFY_FAILURE").as_deref() == Ok("1") {
        fs::write(destination.join("SKILL.md"), "post-install mutation\n")?;
    }
    let installed_identity = directory_identity(&destination).ok();
    let library_unchanged = directory_identity(&a.library)
        .map(|identity| identity == library_identity_before)
        .unwrap_or(false);
    let installed_hash_matches = hash_dir(&destination)
        .map(|hash| hash == incoming_hash)
        .unwrap_or(false);
    if !library_unchanged || !installed_hash_matches {
        let still_owned = installed_identity.is_some()
            && directory_identity(&destination).ok() == installed_identity
            && hash_dir(&destination).ok().as_deref() == Some(&incoming_hash);
        if still_owned {
            let _ = fs::remove_dir_all(&destination);
        }
        return Err(anyhow!(
            "canonical library changed during import; installation aborted{}",
            if still_owned {
                " and package rolled back"
            } else {
                "; recovery required"
            }
        ));
    }
    let previous_state = a.state.clone();
    a.state.local_adoptions.insert(
        key,
        LocalAdoption {
            skill: name.clone(),
            source_path: source.display().to_string(),
            source_package: rel.clone(),
            content_hash: incoming_hash.clone(),
            local_path: destination.display().to_string(),
            status: "adopted".into(),
        },
    );
    if let Err(error) = a.save() {
        a.state = previous_state;
        let still_owned = installed_identity.is_some()
            && directory_identity(&destination).ok() == installed_identity
            && hash_dir(&destination).ok().as_deref() == Some(&incoming_hash);
        return match still_owned.then(|| fs::remove_dir_all(&destination)) {
            Some(Ok(())) => Err(error).context("persist local adoption state; package rolled back"),
            Some(Err(rollback_error)) => Err(anyhow!("persist local adoption state failed: {error}; package rollback failed: {rollback_error}")),
            None => Err(anyhow!("persist local adoption state failed: {error}; installed package changed; recovery required")),
        };
    }
    Ok(
        serde_json::json!({"skill":name,"status":"adopted","source_package":rel,"canonical_path":destination}),
    )
}

pub(crate) fn delete_skill(a: &mut App, raw_skill: &str, yes: bool) -> Result<serde_json::Value> {
    let skill = strict_component(raw_skill, "skill name")?;
    if !yes {
        return Err(anyhow!(
            "confirmation required: pass --yes to delete a canonical library skill"
        ));
    }
    let path = a.library.join(&skill);
    let relative = path
        .strip_prefix(&a.library)
        .map_err(|_| anyhow!("canonical package escaped library"))?;
    assert_no_symlink_path(&a.library, relative)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("skill already_absent: {skill}"))
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || fs::canonicalize(&path)? != path {
        return Err(anyhow!("canonical package is not a regular directory"));
    }
    checked_regular_path(&path.join("SKILL.md"), "skill manifest")?;
    if a.state.subscriptions.values().any(|s| s.skill == skill) {
        return Err(anyhow!(
            "cannot delete {skill}: remove its subscription first"
        ));
    }
    if a.state.publications.values().any(|p| p.skill == skill)
        || a.state
            .pending_publications
            .values()
            .any(|p| p.publication.skill == skill)
    {
        return Err(anyhow!("cannot delete {skill}: unpublish it first"));
    }
    if a.state
        .harness_links
        .values()
        .any(|link| link.skill == skill)
    {
        return Err(anyhow!(
            "cannot delete {skill}: unlink its harness links first"
        ));
    }
    if a.state.sets.values().any(|set| {
        set.members
            .iter()
            .any(|m| set_member_name(m).map(|n| n == skill).unwrap_or(false))
    }) {
        return Err(anyhow!(
            "cannot delete {skill}: remove it from every set first"
        ));
    }
    assert_no_symlink_path(&a.recovery, Path::new("."))?;
    fs::create_dir_all(&a.recovery)?;
    let recovery_path = a
        .recovery
        .join(format!("delete-{skill}-{}", unique_stamp()));
    validate_state_path(&a.recovery, &recovery_path, "recovery")?;
    fs::create_dir(&recovery_path)?;
    let snapshot = recovery_path.join("package");
    copy_complete_tree(&path, &snapshot)?;
    let hash = hash_dir(&snapshot)?;
    if hash_dir(&path)? != hash {
        return Err(anyhow!(
            "canonical package changed while staging deletion; recovery retained"
        ));
    }
    let quarantine = recovery_path.join("quarantine");
    install_dir_noreplace(&path, &quarantine)?;
    let quarantined_hash = hash_dir(&quarantine)?;
    if quarantined_hash != hash || hash_dir(&snapshot)? != hash {
        let _ = install_dir_noreplace(&quarantine, &path);
        return Err(anyhow!(
            "canonical package changed while quarantining deletion; recovery retained"
        ));
    }
    let quarantine_identity = directory_identity(&quarantine)?;
    let previous_state = a.state.clone();
    a.state.local_adoptions.remove(&format!("local:{skill}"));
    if let Err(error) = a.save() {
        a.state = previous_state;
        return match install_dir_noreplace(&quarantine, &path) {
            Ok(()) => Err(error).context("persist deletion state; package restored"),
            Err(restore_error) => Err(anyhow!("persist deletion state failed: {error}; package restore failed: {restore_error}; recovery retained")),
        };
    }
    #[cfg(unix)]
    if std::env::var("SKILLSYNC_TEST_DANGLING_QUARANTINE_SYMLINK").as_deref() == Ok("1") {
        fs::remove_dir_all(&quarantine)?;
        std::os::unix::fs::symlink("missing-quarantine-target", &quarantine)?;
    }
    remove_owned_directory(&a.recovery, &quarantine, Some(quarantine_identity), &hash)?;
    Ok(
        serde_json::json!({"skill":skill,"status":"deleted","recovery_path":recovery_path,"snapshot_hash":hash,"canonical_retained":false}),
    )
}

pub(crate) fn restore_skill(
    a: &App,
    input: &Path,
    requested_skill: Option<&str>,
) -> Result<serde_json::Value> {
    if !input.is_absolute() {
        return Err(anyhow!(
            "recovery path must be absolute and under the configured recovery root"
        ));
    }
    assert_no_symlink_path(&a.recovery, Path::new("."))?;
    // Validate the caller's spelling before canonicalization: a path alias to a
    // valid recovery directory is not an accepted recovery directory.
    assert_no_symlink_path(input, Path::new("."))?;
    let input_meta = fs::symlink_metadata(input).context("recovery snapshot does not exist")?;
    if input_meta.file_type().is_symlink() || !input_meta.is_dir() {
        return Err(anyhow!("recovery snapshot must be a regular directory"));
    }
    let recovery = fs::canonicalize(input).context("recovery snapshot does not exist")?;
    validate_state_path(&a.recovery, &recovery, "recovery")?;
    let recovery_rel = recovery
        .strip_prefix(&a.recovery)
        .map_err(|_| anyhow!("recovery path escaped configured recovery root"))?;
    if recovery_rel.components().count() != 1
        || !recovery_rel
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(anyhow!(
            "recovery path must be a direct child of the configured recovery root"
        ));
    }
    let metadata = fs::symlink_metadata(&recovery)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(anyhow!("recovery snapshot must be a regular directory"));
    }
    let package = recovery.join("package");
    validate_state_path(&a.recovery, &package, "recovery package")?;
    let package_meta = fs::symlink_metadata(&package)
        .map_err(|_| anyhow!("recovery snapshot is missing package"))?;
    if !package_meta.is_dir() || package_meta.file_type().is_symlink() {
        return Err(anyhow!(
            "recovery snapshot package must be a regular directory"
        ));
    }
    let inferred = manifest_name(&package)?;
    let skill = match requested_skill {
        Some(raw) => {
            let name = strict_component(raw, "skill name")?;
            if name != inferred {
                return Err(anyhow!(
                    "explicit skill does not match recovery SKILL.md manifest"
                ));
            }
            name
        }
        None => inferred,
    };
    let destination = a.library.join(&skill);
    let relative = destination
        .strip_prefix(&a.library)
        .map_err(|_| anyhow!("restore destination escaped library"))?;
    assert_no_symlink_path(&a.library, relative)?;
    if recovery == destination
        || recovery.starts_with(&destination)
        || destination.starts_with(&recovery)
    {
        return Err(anyhow!("recovery source and canonical destination overlap"));
    }
    let staging_parent = tempfile::tempdir_in(&a.library)?;
    let staged = staging_parent.path().join("package");
    let source_hash_before = hash_dir(&package)?;
    copy_complete_tree(&package, &staged)?;
    let hash = hash_dir(&staged)?;
    if source_hash_before != hash || hash_dir(&package)? != hash {
        return Err(anyhow!(
            "recovery package changed while staging; restore aborted"
        ));
    }
    fn existing_canonical_package(path: &Path, library: &Path, relative: &Path) -> Result<bool> {
        assert_no_symlink_path(library, relative)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                Err(anyhow!("canonical destination is not a regular directory"))
            }
            Ok(_) => {
                if fs::canonicalize(path)? != path {
                    return Err(anyhow!("canonical destination is not canonical"));
                }
                if !checked_regular_path(&path.join("SKILL.md"), "canonical manifest")? {
                    return Err(anyhow!("canonical destination has no regular SKILL.md"));
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    if existing_canonical_package(&destination, &a.library, relative)? {
        let existing_hash =
            hash_dir(&destination).context("validate existing canonical package before restore")?;
        if existing_hash == hash {
            return Ok(
                serde_json::json!({"skill":skill,"status":"already_present","recovery_path":recovery,"canonical_path":destination,"relationships_recreated":false}),
            );
        }
        return Err(anyhow!(
            "canonical package already exists with different contents; no overwrite performed"
        ));
    }
    fs::create_dir_all(&a.library)?;
    if hash_dir(&package)? != hash {
        return Err(anyhow!("recovery package changed before installation"));
    }
    if std::env::var("SKILLSYNC_TEST_RESTORE_COLLISION").as_deref() == Ok("1") {
        fs::create_dir_all(&destination)?;
        fs::write(destination.join("SKILL.md"), "external collision\n")?;
    }
    install_dir_noreplace(&staged, &destination)
        .context("install restored canonical package without replacement")?;
    let installed_identity = directory_identity(&destination)?;
    if hash_dir(&destination)? != hash {
        let cleanup =
            remove_owned_directory(&a.library, &destination, Some(installed_identity), &hash);
        return Err(match cleanup {
            Ok(()) => {
                anyhow!("restored package failed post-install validation and was rolled back")
            }
            Err(error) => anyhow!("restored package failed post-install validation; {error}"),
        });
    }
    Ok(
        serde_json::json!({"skill":skill,"status":"restored","recovery_path":recovery,"canonical_path":destination,"snapshot_hash":hash,"relationships_recreated":false}),
    )
}
