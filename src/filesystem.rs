use crate::{unique_stamp, App};
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::io::Read;
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

pub(crate) fn effective_library_path(configured: Option<PathBuf>) -> PathBuf {
    std::env::var_os("SKILLSYNC_LIBRARY")
        .map(PathBuf::from)
        .or(configured)
        .unwrap_or_else(|| {
            std::env::var_os(if cfg!(target_os = "windows") {
                "USERPROFILE"
            } else {
                "HOME"
            })
            .map(|x| PathBuf::from(x).join(".agents/skills"))
            .unwrap_or_else(|| PathBuf::from(".agents/skills"))
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileData {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mode: u32,
}
#[cfg(unix)]
pub(crate) fn open_directory_fd(path: &Path) -> Result<std::os::fd::RawFd> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let root = CString::new("/")?;
    let mut fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    for component in absolute.components() {
        let std::path::Component::Normal(name) = component else {
            if matches!(
                component,
                std::path::Component::RootDir | std::path::Component::CurDir
            ) {
                continue;
            }
            unsafe {
                libc::close(fd);
            }
            return Err(anyhow!("unsafe directory path: {}", path.display()));
        };
        let name = match CString::new(name.as_bytes()) {
            Ok(name) => name,
            Err(error) => {
                unsafe {
                    libc::close(fd);
                }
                return Err(error.into());
            }
        };
        let next = unsafe {
            libc::openat(
                fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(error.into());
        }
        unsafe {
            libc::close(fd);
        }
        fd = next;
    }
    Ok(fd)
}

#[cfg(unix)]
pub(crate) fn open_child_file(config: &Path, name: &str, create: bool) -> Result<fs::File> {
    use std::{ffi::CString, os::fd::FromRawFd};
    let directory = open_directory_fd(config)?;
    let name = match CString::new(name.as_bytes()) {
        Ok(name) => name,
        Err(error) => {
            unsafe {
                libc::close(directory);
            }
            return Err(error.into());
        }
    };
    let mut flags = libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if create {
        flags |= libc::O_CREAT;
    }
    let fd = unsafe { libc::openat(directory, name.as_ptr(), flags, 0o600) };
    unsafe {
        libc::close(directory);
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(not(unix))]
pub(crate) fn open_child_file(config: &Path, name: &str, create: bool) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(create);
    Ok(options.open(config.join(name))?)
}

pub(crate) fn open_advisory_lock(config: &Path, name: &str, create: bool) -> Result<fs::File> {
    open_child_file(config, name, create)
}

pub(crate) fn acquire_named_lock(
    config: &Path,
    name: &str,
    busy_message: &str,
) -> Result<fs::File> {
    assert_no_symlink_path(config, Path::new("."))?;
    fs::create_dir_all(config)?;
    assert_no_symlink_path(config, Path::new("."))?;
    let mut file =
        open_advisory_lock(config, name, true).with_context(|| format!("open lock: {name}"))?;
    if let Err(error) = file.try_lock_exclusive() {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(anyhow!("{busy_message}"));
        }
        return Err(error).context("lock Skillsync state");
    }
    file.set_len(0)?;
    file.write_all(format!("pid={}\n", std::process::id()).as_bytes())?;
    file.sync_all()?;
    Ok(file)
}

pub(crate) struct StateLock {
    file: fs::File,
}

impl StateLock {
    pub(crate) fn acquire(config: &Path) -> Result<Self> {
        Ok(Self {
            file: acquire_named_lock(
                config,
                "state.lock",
                "skillsync state is busy (worker or another mutating command holds the lock)",
            )?,
        })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) struct WorkerLease {
    file: fs::File,
}

impl WorkerLease {
    pub(crate) fn acquire(config: &Path) -> Result<Self> {
        Ok(Self {
            file: acquire_named_lock(
                config,
                "worker.active",
                "a Skillsync worker is already running",
            )?,
        })
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) fn resolve_library_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    assert_no_symlink_path(&absolute, Path::new("."))?;
    if absolute.exists() {
        Ok(fs::canonicalize(absolute)?)
    } else {
        Ok(absolute)
    }
}
pub(crate) fn atomic(p: &Path, b: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        atomic_unix(p, b)
    }
    #[cfg(not(unix))]
    {
        atomic_portable(p, b)
    }
}

#[cfg(unix)]
pub(crate) fn atomic_unix(p: &Path, b: &[u8]) -> Result<()> {
    use std::{ffi::CString, os::fd::FromRawFd, os::unix::ffi::OsStrExt};
    let parent = p
        .parent()
        .ok_or_else(|| anyhow!("atomic path has no parent: {}", p.display()))?;
    let target_name = p
        .file_name()
        .ok_or_else(|| anyhow!("atomic path has no name: {}", p.display()))?;
    let target_name = CString::new(target_name.as_bytes())?;
    let temp_name = format!(
        ".{}.tmp-{}-{}",
        p.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        unique_stamp()
    );
    let temp_name = CString::new(temp_name.as_bytes())?;
    assert_no_symlink_path(parent, Path::new("."))?;
    fs::create_dir_all(parent)?;
    assert_no_symlink_path(parent, Path::new("."))?;
    let directory = open_directory_fd(parent)?;
    let fd = unsafe {
        libc::openat(
            directory,
            temp_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(directory);
        }
        return Err(error.into());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    let write_result = (|| {
        file.write_all(b)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        unsafe {
            libc::unlinkat(directory, temp_name.as_ptr(), 0);
            libc::close(directory);
        }
        return Err(error);
    }
    let renamed = unsafe {
        libc::renameat(
            directory,
            temp_name.as_ptr(),
            directory,
            target_name.as_ptr(),
        )
    };
    if renamed < 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::unlinkat(directory, temp_name.as_ptr(), 0);
            libc::close(directory);
        }
        return Err(error.into());
    }
    let synced = unsafe { libc::fsync(directory) };
    let sync_error = if synced < 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        libc::close(directory);
    }
    if let Some(error) = sync_error {
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn atomic_portable(p: &Path, b: &[u8]) -> Result<()> {
    let parent = p
        .parent()
        .ok_or_else(|| anyhow!("atomic path has no parent: {}", p.display()))?;
    assert_no_symlink_path(parent, Path::new("."))?;
    fs::create_dir_all(parent)?;
    assert_no_symlink_path(parent, Path::new("."))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}-{}",
        p.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        unique_stamp()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(b)?;
    file.sync_all()?;
    #[cfg(windows)]
    {
        use std::{iter, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source = temp
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect::<Vec<_>>();
        let target = p
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect::<Vec<_>>();
        let moved = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            let error = std::io::Error::last_os_error();
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        if let Err(error) = fs::rename(&temp, p) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        Ok(())
    }
}
pub(crate) fn safe(p: &Path) -> bool {
    p.is_relative()
        && !p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}
pub(crate) fn strict_component(s: &str, what: &str) -> Result<String> {
    if s.is_empty()
        || s == "."
        || s == ".."
        || s.chars().any(|c| c.is_control())
        || s.chars()
            .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
        || s.contains('/')
        || s.contains('\\')
        || s.contains(':')
        || s.ends_with('.')
        || s.ends_with(' ')
        || windows_reserved_component(s)
        || Path::new(s).is_absolute()
    {
        return Err(anyhow!("invalid {what}: {s:?}"));
    }
    Ok(s.to_owned())
}
pub(crate) fn windows_reserved_component(s: &str) -> bool {
    let base = s.split('.').next().unwrap_or(s);
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
}
pub(crate) fn source_rel(s: &str) -> Result<String> {
    if s.is_empty() || s.chars().any(|c| c.is_control()) || s.contains('\\') || s.contains(':') {
        return Err(anyhow!("unsafe source-relative path: {s}"));
    }
    let p = Path::new(s);
    if s == "." {
        return Ok(s.to_owned());
    }
    if !safe(p) || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(anyhow!("unsafe source-relative path: {s}"));
    }
    Ok(s.to_owned())
}
pub(crate) fn operational(p: &Path) -> bool {
    p.components().any(|c|matches!(c,Component::Normal(x) if matches!(x.to_str(),Some(".git"|".cache"|"logs"|"credentials"|"private"))))||p.file_name().and_then(|x|x.to_str()).map(|x|x==".env"||x.ends_with(".pem")||x.ends_with(".key")).unwrap_or(false)
}
pub(crate) fn portable_rel(path: &Path) -> String {
    if path == Path::new(".") {
        return ".".into();
    }
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
pub(crate) fn read_regular_file(path: &Path, expected: Option<&fs::Metadata>) -> Result<Vec<u8>> {
    reject_reparse_point(path, "regular file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(anyhow!("regular file required: {}", path.display()));
        }
        if let Some(before) = expected {
            if before.dev() != metadata.dev() || before.ino() != metadata.ino() {
                return Err(anyhow!("file changed during scan: {}", path.display()));
            }
        }
        let mut reader = file;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(anyhow!("regular file required: {}", path.display()));
        }
        if expected.is_some() && metadata.len() != expected.unwrap().len() {
            return Err(anyhow!("file changed during scan: {}", path.display()));
        }
        Ok(fs::read(path)?)
    }
}
pub(crate) fn files(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>, u32)>> {
    if fs::symlink_metadata(root)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(anyhow!("symlink package root rejected: {}", root.display()));
    }
    reject_reparse_point(root, "package root")?;
    fn r(root: &Path, p: &Path, o: &mut Vec<(PathBuf, Vec<u8>, u32)>) -> Result<()> {
        for e in fs::read_dir(p)? {
            let e = e?;
            let x = e.path();
            reject_reparse_point(&x, "package entry")?;
            let rel = x.strip_prefix(root)?;
            if operational(rel) {
                continue;
            }
            let m = fs::symlink_metadata(&x)?;
            if m.file_type().is_symlink() {
                return Err(anyhow!("symlink rejected: {}", rel.display()));
            }
            if m.is_dir() {
                r(root, &x, o)?
            } else if m.is_file() {
                #[cfg(unix)]
                use std::os::unix::fs::PermissionsExt;
                #[cfg(unix)]
                let mode = m.permissions().mode();
                #[cfg(not(unix))]
                let mode = 0;
                o.push((rel.to_path_buf(), read_regular_file(&x, Some(&m))?, mode));
            }
        }
        Ok(())
    }
    let mut o = vec![];
    r(root, root, &mut o)?;
    o.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(o)
}
pub(crate) fn write_file_data(path: &Path, data: &FileData) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &data.bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(data.mode))?;
    }
    Ok(())
}
pub(crate) fn hash_dir(p: &Path) -> Result<String> {
    let mut h = Sha256::new();
    for (r, b, m) in files(p)? {
        h.update(r.to_string_lossy().replace('\\', "/").as_bytes());
        h.update([0]);
        h.update(m.to_le_bytes());
        h.update([0]);
        h.update(&b);
        h.update([0]);
    }
    Ok(format!("{:x}", h.finalize()))
}
pub(crate) fn assert_no_symlink_path(root: &Path, rel: &Path) -> Result<()> {
    let mut ancestor = root;
    loop {
        reject_reparse_point(ancestor, "destination ancestor")?;
        if fs::symlink_metadata(ancestor)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(anyhow!(
                "symlink destination ancestor rejected: {}",
                ancestor.display()
            ));
        }
        let Some(parent) = ancestor.parent() else {
            break;
        };
        if parent == ancestor {
            break;
        }
        ancestor = parent;
    }
    if fs::symlink_metadata(root)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(anyhow!(
            "symlink destination root rejected: {}",
            root.display()
        ));
    }
    if rel == Path::new(".") {
        return Ok(());
    }
    let mut current = root.to_path_buf();
    for component in rel.components() {
        let Component::Normal(name) = component else {
            return Err(anyhow!("unsafe destination path: {}", rel.display()));
        };
        current.push(name);
        reject_reparse_point(&current, "destination component")?;
        if fs::symlink_metadata(&current)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(anyhow!(
                "symlink destination component rejected: {}",
                rel.display()
            ));
        }
    }
    Ok(())
}
#[cfg(windows)]
pub(crate) fn reject_reparse_point(path: &Path, label: &str) -> Result<()> {
    use std::{iter, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, FILE_ATTRIBUTE_REPARSE_POINT, INVALID_FILE_ATTRIBUTES,
    };
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error.into());
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(anyhow!(
            "reparse point {label} path rejected: {}",
            path.display()
        ));
    }
    Ok(())
}
#[cfg(not(windows))]
pub(crate) fn reject_reparse_point(_path: &Path, _label: &str) -> Result<()> {
    Ok(())
}
pub(crate) fn checked_regular_path(path: &Path, label: &str) -> Result<bool> {
    reject_reparse_point(path, label)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(anyhow!("symlink {label} path rejected: {}", path.display()))
        }
        Ok(metadata) if !metadata.is_file() => Err(anyhow!(
            "{label} path is not a regular file: {}",
            path.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
pub(crate) fn validate_state_path(root: &Path, target: &Path, label: &str) -> Result<()> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| anyhow!("{label} escaped state directory"))?;
    if relative.as_os_str().is_empty() || !safe(relative) {
        return Err(anyhow!("unsafe {label} path: {}", target.display()));
    }
    assert_no_symlink_path(root, relative)
}
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    assert_no_symlink_path(dst, Path::new("."))?;
    fs::create_dir_all(dst)?;
    for (r, b, m) in files(src)? {
        if !safe(&r) {
            return Err(anyhow!("unsafe path"));
        }
        assert_no_symlink_path(dst, &r)?;
        let d = dst.join(&r);
        fs::create_dir_all(d.parent().unwrap())?;
        fs::write(&d, b)?;
        #[cfg(not(unix))]
        let _ = m;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(d, fs::Permissions::from_mode(m))?;
        }
    }
    Ok(())
}
/// Complete copy used only for deletion recovery; operational names are data.
pub(crate) fn copy_complete_tree(src: &Path, dst: &Path) -> Result<()> {
    reject_reparse_point(src, "delete snapshot source")?;
    let metadata = fs::symlink_metadata(src)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!("delete snapshot source is not a regular directory"));
    }
    assert_no_symlink_path(dst, Path::new("."))?;
    fs::create_dir_all(dst)?;
    fn recurse(root: &Path, current: &Path, dst: &Path) -> Result<()> {
        for entry in fs::read_dir(current)? {
            let source = entry?.path();
            let relative = source.strip_prefix(root)?;
            if !safe(relative) {
                return Err(anyhow!(
                    "unsafe delete snapshot path: {}",
                    relative.display()
                ));
            }
            reject_reparse_point(&source, "delete snapshot entry")?;
            let metadata = fs::symlink_metadata(&source)?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!(
                    "symlink delete snapshot entry rejected: {}",
                    relative.display()
                ));
            }
            let target = dst.join(relative);
            if metadata.is_dir() {
                fs::create_dir_all(&target)?;
                recurse(root, &source, dst)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        &target,
                        fs::Permissions::from_mode(metadata.permissions().mode()),
                    )?;
                }
            } else if metadata.is_file() {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Do not use path-following fs::copy for recovery snapshots.
                // Reopen the final component with no-follow and verify identity.
                assert_no_symlink_path(root, relative)?;
                let bytes = read_regular_file(&source, Some(&metadata))?;
                fs::write(&target, bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        &target,
                        fs::Permissions::from_mode(metadata.permissions().mode()),
                    )?;
                }
            } else {
                return Err(anyhow!(
                    "unsupported delete snapshot entry: {}",
                    relative.display()
                ));
            }
        }
        Ok(())
    }
    recurse(src, src, dst)
}
pub(crate) fn copy_existing_tree(src: &Path, dst: &Path) -> Result<()> {
    reject_reparse_point(src, "destination tree root")?;
    if fs::symlink_metadata(src)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(anyhow!("symlink tree rejected: {}", src.display()));
    }
    assert_no_symlink_path(dst, Path::new("."))?;
    fs::create_dir_all(dst)?;
    fn recurse(root: &Path, current: &Path, dst: &Path) -> Result<()> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let source = entry.path();
            let relative = source.strip_prefix(root)?;
            reject_reparse_point(&source, "destination tree")?;
            if operational(relative)
                && relative
                    .components()
                    .any(|component| matches!(component, Component::Normal(name) if name == ".git"))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&source)?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!(
                    "symlink tree entry rejected: {}",
                    relative.display()
                ));
            }
            if !safe(relative) {
                return Err(anyhow!("unsafe destination path: {}", relative.display()));
            }
            let target = dst.join(relative);
            assert_no_symlink_path(dst, relative)?;
            if metadata.is_dir() {
                fs::create_dir_all(&target)?;
                recurse(root, &source, dst)?;
            } else if metadata.is_file() {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, read_regular_file(&source, Some(&metadata))?)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        &target,
                        fs::Permissions::from_mode(metadata.permissions().mode()),
                    )?;
                }
            }
        }
        Ok(())
    }
    recurse(src, src, dst)
}
pub(crate) fn snapshot(a: &App, skill: &str, src: &Path) -> Result<(PathBuf, String)> {
    strict_component(skill, "baseline key")?;
    assert_no_symlink_path(&a.baselines, Path::new("."))?;
    fs::create_dir_all(&a.baselines)?;
    assert_no_symlink_path(&a.baselines, Path::new("."))?;
    let p = a.baselines.join(skill);
    validate_state_path(&a.baselines, &p, "baseline")?;
    if fs::symlink_metadata(src)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(anyhow!(
            "symlink snapshot source rejected: {}",
            src.display()
        ));
    }
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let staging_parent = tempfile::tempdir_in(p.parent().unwrap_or(Path::new(".")))?;
    let staged = staging_parent.path().join("snapshot");
    copy_tree(src, &staged)?;
    let hash = hash_dir(&staged)?;
    replace_dir(&p, &staged)?;
    Ok((p, hash))
}
#[cfg(target_os = "linux")]
pub(crate) fn install_dir_noreplace(src: &Path, dst: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let parent = dst
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    let name = dst
        .file_name()
        .ok_or_else(|| anyhow!("destination has no name"))?;
    let source_c = CString::new(src.as_os_str().as_bytes())?;
    let name_c = CString::new(name.as_bytes())?;
    let directory = open_directory_fd(parent)?;
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source_c.as_ptr(),
            directory,
            name_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    let result = if status < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    };
    unsafe {
        libc::close(directory);
    }
    result
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn install_dir_noreplace(_src: &Path, _dst: &Path) -> Result<()> {
    Err(anyhow!(
        "safe no-replace directory installation is unavailable on this Unix platform"
    ))
}

#[cfg(windows)]
pub(crate) fn install_dir_noreplace(src: &Path, dst: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, MoveFileExW, INVALID_FILE_ATTRIBUTES, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();

    // MOVEFILE_REPLACE_EXISTING is intentionally absent.  MoveFileExW without
    // that flag provides the kernel's atomic "destination must not exist"
    // rename for this same-volume staged-directory move.  The preflight is
    // only for a useful early error; the syscall remains the race-safe check.
    let destination_exists =
        unsafe { GetFileAttributesW(destination.as_ptr()) } != INVALID_FILE_ATTRIBUTES;
    if destination_exists {
        return Err(anyhow!(
            "destination collision; existing content was not overwritten: {}",
            dst.display()
        ));
    }

    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(2 | 3 | 80 | 183)) {
            return Err(anyhow!(
                "destination collision; existing content was not overwritten: {}",
                dst.display()
            ));
        }
        return Err(error.into());
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn rename_staged_dir(src: &Path, dst: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let parent = dst
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    let name = dst
        .file_name()
        .ok_or_else(|| anyhow!("destination has no name"))?;
    let source_c = CString::new(src.as_os_str().as_bytes())?;
    let name_c = CString::new(name.as_bytes())?;
    let directory = open_directory_fd(parent)?;
    let result = {
        let status = unsafe {
            libc::renameat(
                libc::AT_FDCWD,
                source_c.as_ptr(),
                directory,
                name_c.as_ptr(),
            )
        };
        if status < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    };
    unsafe {
        libc::close(directory);
    }
    result
}
#[cfg(not(unix))]
pub(crate) fn rename_staged_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::rename(src, dst)?;
    Ok(())
}
pub(crate) fn replace_dir(dst: &Path, src: &Path) -> Result<()> {
    assert_no_symlink_path(dst, Path::new("."))?;
    let existing = match fs::symlink_metadata(dst) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(anyhow!("destination is not a regular directory"));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let old = dst.with_extension(format!("old-{}", unique_stamp()));
    if existing {
        rename_staged_dir(dst, &old)?;
    }
    if let Err(error) = rename_staged_dir(src, dst) {
        if existing {
            let _ = rename_staged_dir(&old, dst);
        }
        return Err(error);
    }
    if existing {
        fs::remove_dir_all(old)?;
    }
    Ok(())
}
pub(crate) fn sync_managed_tree(src: &Path, dst: &Path) -> Result<Vec<PathBuf>> {
    let source = files(src)?;
    let source_paths = source
        .iter()
        .map(|(path, _, _)| path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut removed = Vec::new();
    if dst.exists() {
        for (path, _, _) in files(dst)? {
            if !source_paths.contains(&path) {
                assert_no_symlink_path(dst, &path)?;
                fs::remove_file(dst.join(&path))?;
                removed.push(path);
            }
        }
    } else {
        assert_no_symlink_path(dst, Path::new("."))?;
        fs::create_dir_all(dst)?;
    }
    for (path, bytes, mode) in source {
        if !safe(&path) {
            return Err(anyhow!("unsafe publication path: {}", path.display()));
        }
        assert_no_symlink_path(dst, &path)?;
        let target = dst.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, bytes)?;
        #[cfg(not(unix))]
        let _ = mode;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(target, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(removed)
}
pub(crate) fn discover(root: &Path) -> Result<Vec<(String, PathBuf, String)>> {
    reject_reparse_point(root, "discovery root")?;
    fn r(base: &Path, p: &Path, o: &mut Vec<(String, PathBuf, String)>) -> Result<()> {
        for e in fs::read_dir(p)? {
            let x = e?.path();
            reject_reparse_point(&x, "discovery entry")?;
            let rel = x.strip_prefix(base)?;
            if operational(rel) {
                continue;
            }
            let metadata = fs::symlink_metadata(&x)?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!("symlink package path rejected: {}", rel.display()));
            }
            if metadata.is_dir() {
                let manifest = x.join("SKILL.md");
                reject_reparse_point(&manifest, "manifest")?;
                if manifest.is_file() {
                    let name = manifest_name(&x)?;
                    o.push((name, x.clone(), portable_rel(rel)))
                }
                r(base, &x, o)?
            }
        }
        Ok(())
    }
    let mut o = vec![];
    let root_manifest = root.join("SKILL.md");
    reject_reparse_point(&root_manifest, "manifest")?;
    if root_manifest.is_file() {
        let name = manifest_name(root)?;
        o.push((name, root.to_path_buf(), ".".into()))
    }
    r(root, root, &mut o)?;
    o.sort_by(|a, b| a.2.cmp(&b.2));
    Ok(o)
}
pub(crate) fn manifest_name(p: &Path) -> Result<String> {
    let text = String::from_utf8(read_regular_file(&p.join("SKILL.md"), None)?)
        .context("SKILL.md is not valid UTF-8")?;
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let Some(value) = first.strip_prefix("name:") else {
        return Err(anyhow!(
            "malformed SKILL.md front matter: first line must be name: <safe-component>"
        ));
    };
    if value.is_empty() || !value.as_bytes()[0].is_ascii_whitespace() {
        return Err(anyhow!("malformed SKILL.md front matter name"));
    }
    let value = value.trim();
    if value.is_empty()
        || value.starts_with('"')
        || value.starts_with('\'')
        || value.contains(':')
        || lines.any(|line| line.trim_start().starts_with("name:"))
    {
        return Err(anyhow!("malformed SKILL.md front matter name"));
    }
    strict_component(value, "manifest skill name")
}
