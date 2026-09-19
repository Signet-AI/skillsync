use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use crate::filesystem::open_entry_checked;
#[cfg(unix)]
use crate::filesystem::read_directory_entries;
use crate::filesystem::{
    assert_no_symlink_path, canonicalize_path, checked_regular_path, copy_complete_tree, copy_tree,
    discover, hash_dir, install_dir_noreplace, manifest_name, reject_reparse_point,
    strict_component, validate_state_path,
};
use crate::*;

#[cfg(unix)]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedConflictManifest {
    manifest_version: u32,
    relationship: String,
    source: String,
    source_path: String,
    path: String,
    base_hash: String,
    base_path: String,
    local_hash: String,
    local_path: String,
    incoming_hash: String,
    incoming_path: String,
    live_hash_at_detection: String,
    baseline_path: String,
    baseline_source: String,
    baseline_source_path: String,
    transition: String,
    status: String,
}

#[cfg(unix)]
fn retained_tree_hash(root: &fs::File) -> Result<String> {
    use std::{ffi::CString, io::Read, os::fd::AsRawFd};
    let mut files = Vec::new();
    fn walk(dir: &fs::File, rel: &Path, out: &mut Vec<(PathBuf, Vec<u8>, u32)>) -> Result<()> {
        for name in read_directory_entries(dir.as_raw_fd())? {
            let c = CString::new(name.as_encoded_bytes())?;
            let child = open_entry_checked(dir.as_raw_fd(), &c, Path::new("conflict evidence"))?;
            let meta = child.metadata()?;
            let child_rel = rel.join(&name);
            if crate::filesystem::operational(&child_rel) {
                continue;
            }
            if meta.is_dir() {
                walk(&child, &child_rel, out)?;
            } else if meta.is_file() {
                let mut bytes = Vec::new();
                (&child).read_to_end(&mut bytes)?;
                #[cfg(unix)]
                use std::os::unix::fs::PermissionsExt;
                out.push((child_rel, bytes, meta.permissions().mode()));
            } else {
                return Err(anyhow!("conflict evidence contains unsupported entry"));
            }
        }
        Ok(())
    }
    walk(root, Path::new(""), &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (path, bytes, mode) in files {
        h.update(path.to_string_lossy().replace('\\', "/").as_bytes());
        h.update([0]);
        h.update(mode.to_le_bytes());
        h.update([0]);
        h.update(bytes);
        h.update([0]);
    }
    Ok(format!("{:x}", h.finalize()))
}

#[cfg(unix)]
fn validate_retained_conflict(
    root: &fs::File,
    name: &std::ffi::OsStr,
    relationship: &str,
    subscription: &crate::Subscription,
) -> Result<serde_json::Value> {
    use std::{ffi::CString, io::Read, os::fd::AsRawFd, os::unix::ffi::OsStrExt};
    let direct = CString::new(name.as_bytes())?;
    let artifact = open_entry_checked(
        root.as_raw_fd(),
        &direct,
        Path::new("conflict recovery artifact"),
    )?;
    if !artifact.metadata()?.is_dir() {
        return Err(anyhow!("conflict recovery artifact is not a directory"));
    }
    for entry in read_directory_entries(artifact.as_raw_fd())? {
        let allowed = matches!(
            entry.to_str(),
            Some("manifest.json") | Some("base") | Some("local") | Some("incoming")
        );
        if !allowed {
            return Err(anyhow!(
                "conflict recovery artifact contains unknown top-level entry"
            ));
        }
        let entry_name = CString::new(entry.as_bytes())?;
        let entry_file = open_entry_checked(
            artifact.as_raw_fd(),
            &entry_name,
            Path::new("conflict recovery artifact entry"),
        )?;
        let metadata = entry_file.metadata()?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(anyhow!(
                "conflict recovery artifact contains unsupported entry"
            ));
        }
    }
    let manifest_name = CString::new("manifest.json")?;
    let manifest_file = open_entry_checked(
        artifact.as_raw_fd(),
        &manifest_name,
        Path::new("conflict manifest"),
    )?;
    if !manifest_file.metadata()?.is_file() {
        return Err(anyhow!("conflict manifest is not a regular file"));
    }
    let mut raw = Vec::new();
    (&manifest_file).read_to_end(&mut raw)?;
    let m: RetainedConflictManifest =
        serde_json::from_slice(&raw).map_err(|_| anyhow!("conflict manifest is malformed"))?;
    if m.manifest_version != 1
        || m.relationship != relationship
        || m.source != subscription.source
        || m.source_path != subscription.source_path
        || m.path != subscription.local_path
        || m.baseline_path != subscription.baseline_path
        || m.baseline_source != subscription.source
        || m.baseline_source_path != subscription.source_path
        || m.transition != "directory"
        || m.status != "open"
    {
        return Err(anyhow!("conflict manifest is incompatible or mismatched"));
    }
    if m.base_hash != subscription.baseline_hash || m.live_hash_at_detection != m.local_hash {
        return Err(anyhow!("conflict evidence identity mismatch"));
    }
    for (label, path, expected) in [
        ("base", m.base_path.as_str(), &m.base_hash),
        ("local", m.local_path.as_str(), &m.local_hash),
        ("incoming", m.incoming_path.as_str(), &m.incoming_hash),
    ] {
        if path
            != Path::new(subscription.recovery_path.as_deref().unwrap_or(""))
                .join(label)
                .display()
                .to_string()
        {
            return Err(anyhow!("conflict {label} path mismatch"));
        }
        let child = open_entry_checked(
            artifact.as_raw_fd(),
            &CString::new(label)?,
            Path::new("conflict evidence side"),
        )?;
        if !child.metadata()?.is_dir() || retained_tree_hash(&child)? != *expected {
            return Err(anyhow!("conflict {label} evidence hash mismatch"));
        }
    }
    Ok(
        serde_json::json!({"category":"conflict","status":"open","reason":"validated_conflict_evidence_retained","deletable":false}),
    )
}

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

#[allow(unreachable_code)]
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
    #[cfg(unix)]
    remove_owned_directory_at(root, relative, current)?;
    #[cfg(windows)]
    crate::filesystem::remove_owned_directory_path_windows(path, current)?;
    #[cfg(not(any(unix, windows)))]
    return Err(anyhow!(
        "safe descriptor-relative removal unavailable on this platform"
    ));
    #[cfg(any(unix, windows))]
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

#[cfg(unix)]
fn remove_owned_directory_at(
    root: &Path,
    relative: &Path,
    expected: DirectoryIdentity,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
        os::unix::ffi::OsStrExt,
    };
    let parts: Vec<_> = relative.components().collect();
    let Some(Component::Normal(last)) = parts.last() else {
        return Err(anyhow!("unsafe cleanup path"));
    };
    let mut parent = crate::filesystem::open_directory_file_bound(root)?;
    for component in &parts[..parts.len() - 1] {
        let Component::Normal(name) = component else {
            return Err(anyhow!("unsafe cleanup path"));
        };
        let name = CString::new(name.as_bytes())?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        parent = unsafe { fs::File::from_raw_fd(fd) };
    }
    let name = CString::new(last.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let target = unsafe { fs::File::from_raw_fd(fd) };
    let meta = target.metadata()?;
    if (meta.dev(), meta.ino()) != expected {
        return Err(anyhow!(
            "cleanup ownership changed; recovery required: {}",
            relative.display()
        ));
    }
    fn recurse(dir: &fs::File) -> Result<()> {
        use std::os::fd::AsRawFd;
        for name in read_directory_entries(dir.as_raw_fd())? {
            let c = CString::new(name.as_encoded_bytes())?;
            let child = open_entry_checked(dir.as_raw_fd(), &c, Path::new("cleanup child"))?;
            let m = child.metadata()?;
            let expected = (m.dev(), m.ino());
            if m.is_dir() {
                recurse(&child)?;
            } else if !m.is_file() {
                return Err(anyhow!("cleanup target contains unsupported entry"));
            }
            // unlinkat has no unlink-by-handle form. Re-open immediately before
            // removal and require the same object; a race retains the artifact.
            let check = open_entry_checked(dir.as_raw_fd(), &c, Path::new("cleanup child"))?;
            let check_meta = check.metadata()?;
            if (check_meta.dev(), check_meta.ino()) != expected
                || check_meta.is_dir() != m.is_dir()
                || (!m.is_dir() && !check_meta.is_file())
            {
                return Err(anyhow!(
                    "cleanup child identity changed; retaining artifact"
                ));
            }
            let flags = if m.is_dir() { libc::AT_REMOVEDIR } else { 0 };
            if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(())
    }
    recurse(&target)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn recovery_inventory_digest(root_file: &fs::File) -> Result<String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::{ffi::CString, io::Read, os::fd::AsRawFd, os::unix::ffi::OsStrExt};
    let mut records: Vec<(PathBuf, fs::Metadata, Option<Vec<u8>>)> = Vec::new();
    fn walk(
        dir: &fs::File,
        rel: &Path,
        records: &mut Vec<(PathBuf, fs::Metadata, Option<Vec<u8>>)>,
    ) -> Result<()> {
        let mut names = read_directory_entries(dir.as_raw_fd())?;
        names.sort();
        for name in names {
            let c_name = CString::new(name.as_bytes())?;
            let child = open_entry_checked(
                dir.as_raw_fd(),
                &c_name,
                Path::new("recovery inventory entry"),
            )?;
            let metadata = child.metadata()?;
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(anyhow!("recovery inventory special entry rejected"));
            }
            let child_rel = rel.join(&name);
            if !child_rel
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                return Err(anyhow!("unsafe recovery inventory path"));
            }
            let bytes = if metadata.is_file() {
                let before = child.metadata()?;
                let mut bytes = Vec::new();
                (&child).take(u64::MAX).read_to_end(&mut bytes)?;
                let after = child.metadata()?;
                if (before.dev(), before.ino()) != (after.dev(), after.ino())
                    || before.len() != after.len()
                {
                    return Err(anyhow!("recovery inventory file changed during read"));
                }
                Some(bytes)
            } else {
                None
            };
            let child_rel_for_walk = child_rel.clone();
            records.push((child_rel, metadata.clone(), bytes));
            if metadata.is_dir() {
                walk(&child, &child_rel_for_walk, records)?;
            }
        }
        Ok(())
    }
    walk(root_file, Path::new(""), &mut records)?;
    records.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    hasher.update(b"skillsync-recovery-inventory-v1\\0");
    for (relative, metadata, bytes) in records {
        let relative_bytes = relative.as_os_str().as_bytes();
        hasher.update((relative_bytes.len() as u64).to_le_bytes());
        hasher.update(relative_bytes);
        hasher.update(if metadata.is_dir() { b"d" } else { b"f" });
        hasher.update(metadata.len().to_le_bytes());
        hasher.update((metadata.permissions().mode() as u64).to_le_bytes());
        if let Some(bytes) = bytes {
            hasher.update(bytes);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(not(unix))]
fn recovery_inventory_digest(root: &Path) -> Result<String> {
    let mut records = Vec::new();
    fn walk(root: &Path, current: &Path, records: &mut Vec<(PathBuf, fs::Metadata)>) -> Result<()> {
        reject_reparse_point(current, "recovery inventory entry")?;
        let metadata = fs::symlink_metadata(current)?;
        if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
            return Err(anyhow!("recovery inventory entry rejected"));
        }
        let relative = current
            .strip_prefix(root)
            .map_err(|_| anyhow!("recovery inventory path escaped root"))?;
        if !relative.as_os_str().is_empty() {
            records.push((relative.to_path_buf(), metadata.clone()));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(current)? {
                walk(root, &entry?.path(), records)?;
            }
        }
        Ok(())
    }
    walk(root, root, &mut records)?;
    records.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    h.update(b"skillsync-recovery-inventory-v1\\0");
    for (p, m) in records {
        h.update(p.as_os_str().as_encoded_bytes());
        h.update(if m.is_dir() { b"d" } else { b"f" });
        if m.is_file() {
            return Err(anyhow!("safe recovery file reads unavailable"));
        }
    }
    Ok(format!("{:x}", h.finalize()))
}

#[allow(dead_code)]
fn validate_recovery_snapshot_tree(root: &Path) -> Result<()> {
    fn walk(root: &Path, current: &Path) -> Result<()> {
        let relative = current
            .strip_prefix(root)
            .map_err(|_| anyhow!("recovery snapshot path escaped root"))?;
        if !relative.as_os_str().is_empty()
            && !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(anyhow!(
                "unsafe recovery snapshot path: {}",
                relative.display()
            ));
        }
        reject_reparse_point(current, "recovery snapshot entry")?;
        let metadata = fs::symlink_metadata(current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(anyhow!("recovery snapshot root is not a regular directory"));
        }
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| anyhow!("recovery snapshot path escaped root"))?;
            if !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            {
                return Err(anyhow!(
                    "unsafe recovery snapshot path: {}",
                    relative.display()
                ));
            }
            reject_reparse_point(&path, "recovery snapshot entry")?;
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!(
                    "recovery snapshot symlink rejected: {}",
                    relative.display()
                ));
            }
            if metadata.is_dir() {
                walk(root, &path)?;
            } else if !metadata.is_file() {
                return Err(anyhow!(
                    "recovery snapshot special entry rejected: {}",
                    relative.display()
                ));
            }
        }
        Ok(())
    }
    walk(root, root)
}

pub(crate) fn list_inventory(a: &mut App) -> Result<serde_json::Value> {
    #[cfg(unix)]
    let root_file = crate::filesystem::open_directory_file_bound(&a.recovery)?;
    #[cfg(unix)]
    let root_identity = {
        use std::os::unix::fs::MetadataExt;
        let m = root_file.metadata()?;
        (m.dev(), m.ino())
    };
    #[cfg(all(unix, feature = "test-hooks"))]
    if std::env::var("SKILLSYNC_TEST_REPLACE_RECOVERY_ROOT").as_deref() == Ok("1") {
        let replacement = a.recovery.with_extension("replaced");
        fs::rename(&a.recovery, &replacement).context("test hook rename recovery root")?;
        fs::create_dir(&a.recovery).context("test hook replace recovery root")?;
    }
    #[cfg(unix)]
    let mut entries = {
        use std::os::fd::AsRawFd;
        read_directory_entries(root_file.as_raw_fd())?
            .into_iter()
            .map(|name| (name, ()))
            .collect::<Vec<_>>()
    };
    #[cfg(not(unix))]
    let mut entries = fs::read_dir(&a.recovery)?
        .map(|e| e.map(|e| (e.file_name(), ())))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut artifacts = Vec::new();
    for (sorted_position, (name, _)) in entries.into_iter().enumerate() {
        #[cfg(not(unix))]
        let path = a.recovery.join(&name);
        #[cfg(unix)]
        let entry = {
            use std::os::unix::ffi::OsStrExt;
            use std::{ffi::CString, os::fd::AsRawFd};
            let c = CString::new(name.as_os_str().as_bytes())?;
            open_entry_checked(
                root_file.as_raw_fd(),
                &c,
                Path::new("recovery inventory entry"),
            )
        };
        #[cfg(not(unix))]
        let entry: Result<()> = Ok(());
        #[cfg(unix)]
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                artifacts.push(serde_json::json!({"id": format!("recovery-{:016x}", sorted_position), "category":"unknown", "status":"invalid", "reason":"unrecognized_recovery_artifact", "deletable":false}));
                continue;
            }
        };
        #[cfg(not(unix))]
        if entry.is_err() {
            artifacts.push(serde_json::json!({"id": format!("recovery-{:016x}", sorted_position), "category":"unknown", "status":"invalid", "reason":"unrecognized_recovery_artifact", "deletable":false}));
            continue;
        }
        #[cfg(unix)]
        let digest = recovery_inventory_digest(&entry);
        #[cfg(not(unix))]
        let digest = recovery_inventory_digest(&path);
        let digest = match digest {
            Ok(v) => v,
            Err(_) => {
                artifacts.push(serde_json::json!({"id": format!("recovery-{:016x}", sorted_position), "category":"unknown", "status":"invalid", "reason":"unrecognized_recovery_artifact", "deletable":false}));
                continue;
            }
        };
        let opaque_id = {
            let mut id_hasher = Sha256::new();
            id_hasher.update(b"skillsync-recovery-opaque-id-v2\\0");
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let name_bytes = name.as_os_str().as_bytes();
                id_hasher.update((name_bytes.len() as u64).to_le_bytes());
                id_hasher.update(name_bytes);
            }
            #[cfg(not(unix))]
            {
                let name_bytes = name.as_os_str().as_encoded_bytes();
                id_hasher.update((name_bytes.len() as u64).to_le_bytes());
                id_hasher.update(name_bytes);
            }
            id_hasher.update(digest.as_bytes());
            let id_digest = format!("{:x}", id_hasher.finalize());
            format!("recovery-{}", &id_digest[..16])
        };
        #[cfg(unix)]
        if let Some((relationship, subscription)) = a.state.subscriptions.iter().find(|(_, s)| {
            s.status == "conflict"
                && s.recovery_path.as_deref().map(Path::new)
                    == Some(a.recovery.join(&name).as_path())
        }) {
            if validate_retained_conflict(&root_file, &name, relationship, subscription).is_ok() {
                artifacts.push(serde_json::json!({"id":opaque_id,"category":"conflict","status":"open","reason":"validated_conflict_evidence_retained","deletable":false}));
                continue;
            }
        }
        // Recovery inventory is deliberately conservative until its complete
        // descriptor-relative manifest/evidence validator has accepted the
        // artifact. Never delegate to conflicts::show here: that validator
        // reads the live subscription and recorded source by pathname.
        let item = serde_json::json!({"id":opaque_id,"category":"unknown","status":"invalid","reason":"unrecognized_recovery_artifact","deletable":false});
        artifacts.push(item);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = fs::symlink_metadata(&a.recovery)?;
        if (current.dev(), current.ino()) != root_identity {
            return Err(anyhow!("recovery root changed during inventory"));
        }
        let retained = root_file.metadata()?;
        if (retained.dev(), retained.ino()) != root_identity {
            return Err(anyhow!("recovery root handle changed"));
        }
    }
    Ok(serde_json::json!({"artifacts": artifacts, "count": artifacts.len()}))
}

pub(crate) fn import_local(
    a: &mut App,
    source: &Path,
    requested: Option<&str>,
) -> Result<serde_json::Value> {
    use std::io::IsTerminal;
    if fs::symlink_metadata(source)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(anyhow!(
            "import source symlink rejected: {}",
            source.display()
        ));
    }
    let source = crate::filesystem::canonicalize_path_with_missing(source)
        .context("canonicalize import source")?;
    assert_no_symlink_path(&source, Path::new("."))?;
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
            let query = crate::repository::skill_query(query)?;
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
            if canonicalize_path(&destination)? != destination {
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
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_IMPORT_COLLISION").as_deref() == Ok("1") {
        fs::create_dir_all(&destination)?;
        fs::write(destination.join("SKILL.md"), "external collision\n")?;
    }
    install_dir_noreplace(&staged, &destination)
        .context("install canonical package without replacement")?;
    #[cfg(feature = "test-hooks")]
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
    if !metadata.is_dir() || canonicalize_path(&path)? != path {
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
    #[cfg(feature = "test-hooks")]
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
    let recovery = canonicalize_path(input).context("recovery snapshot does not exist")?;
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
                if canonicalize_path(path)? != path {
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
    #[cfg(feature = "test-hooks")]
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
