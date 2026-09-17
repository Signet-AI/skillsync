use crate::{unique_stamp, App};
use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
        let mut next = unsafe {
            libc::openat(
                fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            unsafe { libc::mkdirat(fd, name.as_ptr(), 0o755) };
            next = unsafe {
                libc::openat(
                    fd,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
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
fn open_directory_file_no_create(path: &Path) -> Result<fs::File> {
    use std::os::unix::ffi::OsStrExt;
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let root = CString::new("/")?;
    let root_fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut owner = unsafe { fs::File::from_raw_fd(root_fd) };
    for component in absolute.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir | Component::CurDir) {
                continue;
            }
            return Err(anyhow!("unsafe directory path: {}", path.display()));
        };
        let name = CString::new(name.as_bytes())?;
        let next = unsafe {
            libc::openat(
                owner.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        owner = unsafe { fs::File::from_raw_fd(next) };
    }
    Ok(owner)
}

/// Bind a pathname lookup to the directory identity observed immediately before it.
/// The retained descriptor is used after the comparison; replacements fail closed.
#[cfg(not(unix))]
pub(crate) fn open_directory_file_bound(path: &Path) -> Result<fs::File> {
    Err(anyhow!(
        "safe descriptor-relative traversal unavailable on this platform: {}",
        path.display()
    ))
}

#[cfg(unix)]
pub(crate) fn open_directory_file_bound(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::MetadataExt;
    let expected = fs::symlink_metadata(path)?;
    if expected.file_type().is_symlink() || !expected.is_dir() {
        return Err(anyhow!("regular directory required: {}", path.display()));
    }
    let expected = (expected.dev(), expected.ino());
    let opened = open_directory_file_no_create(path)?;
    let actual = opened.metadata()?;
    if !actual.is_dir()
        || actual.file_type().is_symlink()
        || (actual.dev(), actual.ino()) != expected
    {
        return Err(anyhow!("directory changed during open: {}", path.display()));
    }
    Ok(opened)
}

#[cfg(unix)]
fn entry_mode_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
) -> Result<Option<libc::mode_t>> {
    use std::mem::MaybeUninit;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(Some(stat.st_mode & libc::S_IFMT))
}

#[cfg(unix)]
fn verify_bound_parent(path: &Path, expected: &fs::File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    let actual = open_directory_file_bound(parent)?;
    let expected_metadata = expected.metadata()?;
    let actual_metadata = actual.metadata()?;
    if !expected_metadata.is_dir()
        || !actual_metadata.is_dir()
        || (expected_metadata.dev(), expected_metadata.ino())
            != (actual_metadata.dev(), actual_metadata.ino())
    {
        return Err(anyhow!(
            "destination parent changed during replacement: {}",
            parent.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn destination_directory_exists(
    parent: &fs::File,
    name: &std::ffi::CStr,
    display: &Path,
) -> Result<bool> {
    use std::os::fd::AsRawFd;
    let Some(kind) = entry_mode_at(parent.as_raw_fd(), name)? else {
        return Ok(false);
    };
    if kind != libc::S_IFDIR {
        if kind == libc::S_IFLNK {
            return Err(anyhow!(
                "destination symlink rejected: {}",
                display.display()
            ));
        }
        return Err(anyhow!(
            "destination is not a regular directory: {}",
            display.display()
        ));
    }
    let checked = open_entry_checked(parent.as_raw_fd(), name, display)?;
    if !checked.metadata()?.is_dir() {
        return Err(anyhow!(
            "destination changed to a non-directory: {}",
            display.display()
        ));
    }
    Ok(true)
}

#[cfg(unix)]
fn entry_identity_at(
    parent: &fs::File,
    name: &std::ffi::CStr,
    display: &Path,
) -> Result<(u64, u64)> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let entry = open_entry_checked(parent.as_raw_fd(), name, display)?;
    let metadata = entry.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
pub(crate) fn open_entry_checked(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
    display: &Path,
) -> Result<fs::File> {
    use std::os::fd::{FromRawFd, RawFd};
    let Some(kind) = entry_mode_at(parent, name)? else {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
    };
    if kind == libc::S_IFLNK {
        return Err(anyhow!("symlink rejected: {}", display.display()));
    }
    let flags = if kind == libc::S_IFDIR {
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
    } else if kind == libc::S_IFREG {
        // O_NONBLOCK makes the second open safe if the entry is replaced after
        // fstatat and before openat. Regular files ignore this flag.
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
    } else {
        return Err(anyhow!(
            "unsupported special file rejected: {}",
            display.display()
        ));
    };
    let fd: RawFd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    let actual = file.metadata()?;
    let valid =
        (kind == libc::S_IFDIR && actual.is_dir()) || (kind == libc::S_IFREG && actual.is_file());
    if !valid {
        return Err(anyhow!(
            "entry changed to an unsupported type: {}",
            display.display()
        ));
    }
    Ok(file)
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

fn ensure_regular_file(file: &fs::File, label: &str) -> Result<()> {
    if !file.metadata()?.is_file() {
        return Err(anyhow!("{label} is not a regular file"));
    }
    Ok(())
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
    ensure_regular_file(&file, name)?;
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

#[cfg(unix)]
fn open_lock_relative(directory: &fs::File, name: &str, create: bool) -> Result<fs::File> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };
    let name = CString::new(name)?;
    let flags =
        libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW | if create { libc::O_CREAT } else { 0 };
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    ensure_regular_file(&file, "state.lock")?;
    Ok(file)
}

pub(crate) struct StateLock {
    file: fs::File,
    config_identity: ConfigIdentity,
    #[cfg(unix)]
    directory: fs::File,
}

#[derive(Clone, Copy)]
struct ConfigIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(not(unix))]
    len: u64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

fn config_identity(config: &Path) -> Result<ConfigIdentity> {
    let metadata = fs::metadata(config)?;
    if !metadata.is_dir() {
        return Err(anyhow!("config path is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(ConfigIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(ConfigIdentity {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

impl ConfigIdentity {
    fn matches(&self, current: &ConfigIdentity) -> bool {
        #[cfg(unix)]
        {
            self.dev == current.dev && self.ino == current.ino
        }
        #[cfg(not(unix))]
        {
            self.len == current.len && self.modified == current.modified
        }
    }
}

impl StateLock {
    pub(crate) fn acquire(config: &Path) -> Result<Self> {
        #[cfg(unix)]
        let directory = open_directory_file(config)?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            let metadata = directory.metadata()?;
            ConfigIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            }
        };
        #[cfg(unix)]
        let mut file =
            open_lock_relative(&directory, "state.lock", true).context("open lock: state.lock")?;
        #[cfg(not(unix))]
        let (mut file, identity) = (
            acquire_named_lock(
                config,
                "state.lock",
                "skillsync state is busy (worker or another mutating command holds the lock)",
            )?,
            config_identity(config)?,
        );
        if let Err(error) = file.try_lock_exclusive() {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(anyhow!(
                    "skillsync state is busy (worker or another mutating command holds the lock)"
                ));
            }
            return Err(error).context("lock Skillsync state");
        }
        file.set_len(0)?;
        file.write_all(format!("pid={}\n", std::process::id()).as_bytes())?;
        file.sync_all()?;
        Ok(Self {
            file,
            config_identity: identity,
            #[cfg(unix)]
            directory,
        })
    }

    pub(crate) fn acquire_read_only_if_present(config: &Path) -> Result<Option<Self>> {
        let Some((file, identity, _directory)) = open_existing_read_lock(config, "state.lock")?
        else {
            return Ok(None);
        };
        file.try_lock_shared().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => anyhow!(
                "skillsync state is busy (worker or another mutating command holds the lock)"
            ),
            std::fs::TryLockError::Error(error) => {
                anyhow!(error).context("read lock Skillsync state")
            }
        })?;
        Ok(Some(Self {
            file,
            config_identity: identity,
            #[cfg(unix)]
            directory: _directory.ok_or_else(|| anyhow!("missing anchored directory"))?,
        }))
    }

    pub(crate) fn verify_config_identity(&self, config: &Path) -> Result<()> {
        let current = config_identity(config)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let retained = self.directory.metadata()?;
            if retained.dev() != self.config_identity.dev
                || retained.ino() != self.config_identity.ino
            {
                return Err(anyhow!("config directory handle identity changed"));
            }
        }
        if !self.config_identity.matches(&current) {
            return Err(anyhow!(
                "config directory changed during read-only inventory"
            ));
        }
        Ok(())
    }
}

fn open_existing_read_lock(
    config: &Path,
    name: &str,
) -> Result<Option<(fs::File, ConfigIdentity, Option<fs::File>)>> {
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::fd::FromRawFd};
        let directory_file = match open_directory_file_no_create(config) {
            Ok(file) => file,
            Err(error) if error.to_string().contains("No such file") => return Ok(None),
            Err(error) => return Err(error),
        };
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let metadata = directory_file.metadata()?;
        let identity = ConfigIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        };
        let name = CString::new(name.as_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory_file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                drop(directory_file);
                return Ok(None);
            }
            drop(directory_file);
            return Err(error.into());
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        ensure_regular_file(&file, "state.lock")?;
        Ok(Some((file, identity, Some(directory_file))))
    }
    #[cfg(not(unix))]
    {
        match fs::OpenOptions::new().read(true).open(config.join(name)) {
            Ok(file) => Ok(Some((file, config_identity(config)?, None))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

impl StateLock {
    #[cfg(unix)]
    pub(crate) fn directory_try_clone(&self) -> Result<fs::File> {
        Ok(self.directory.try_clone()?)
    }
}

#[cfg(unix)]
pub(crate) fn open_directory_file(path: &Path) -> Result<fs::File> {
    use std::os::fd::FromRawFd;
    Ok(unsafe { fs::File::from_raw_fd(open_directory_fd(path)?) })
}

#[cfg(unix)]
pub(crate) fn read_relative_file(directory: &fs::File, name: &str) -> Result<Vec<u8>> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };
    let name = CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
pub(crate) fn write_relative_file_fd(directory: &fs::File, name: &str, bytes: &[u8]) -> Result<()> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
    };
    let name = CString::new(name)?;
    let temp = CString::new(format!(".state.tmp-{}", std::process::id()))?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    file.sync_all()?;
    let rc = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temp.as_ptr(),
            directory.as_raw_fd(),
            name.as_ptr(),
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {
        libc::fsync(directory.as_raw_fd());
    }
    Ok(())
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
    let mut out = Vec::new();
    #[cfg(unix)]
    fn walk(
        fd: std::os::fd::RawFd,
        rel: &Path,
        out: &mut Vec<(PathBuf, Vec<u8>, u32)>,
    ) -> Result<()> {
        use std::{ffi::CString, os::fd::AsRawFd};
        let entries = fs::read_dir(format!("/proc/self/fd/{fd}"))?;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let child_rel = rel.join(&name);
            if operational(&child_rel) {
                continue;
            }
            let c_name = CString::new(name.as_encoded_bytes())?;
            let child_file = open_entry_checked(fd, &c_name, &child_rel)?;
            let metadata = child_file.metadata()?;
            if metadata.is_dir() {
                walk(child_file.as_raw_fd(), &child_rel, out)?;
                // child_file remains held until the recursive enumeration completes.
            } else if metadata.is_file() {
                let mode = metadata_mode(&metadata);
                let mut reader = child_file;
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes)?;
                out.push((child_rel, bytes, mode));
            } else {
                return Err(anyhow!("unsupported entry: {}", child_rel.display()));
            }
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        let root_file = open_directory_file_bound(root)?;
        let result = walk(
            std::os::fd::AsRawFd::as_raw_fd(&root_file),
            Path::new(""),
            &mut out,
        );
        result?;
    }
    #[cfg(not(unix))]
    {
        fn walk(root: &Path, current: &Path, out: &mut Vec<(PathBuf, Vec<u8>, u32)>) -> Result<()> {
            for entry in fs::read_dir(current)? {
                let path = entry?.path();
                let rel = path.strip_prefix(root)?;
                if operational(rel) {
                    continue;
                }
                reject_reparse_point(&path, "package entry")?;
                let m = fs::symlink_metadata(&path)?;
                if m.file_type().is_symlink() {
                    return Err(anyhow!("symlink rejected: {}", rel.display()));
                }
                if m.is_dir() {
                    walk(root, &path, out)?;
                } else if m.is_file() {
                    out.push((rel.to_path_buf(), read_regular_file(&path, Some(&m))?, 0));
                }
            }
            Ok(())
        }
        walk(root, root, &mut out)?;
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
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
    if root.starts_with("/proc/self/fd") {
        return Ok(());
    }
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
#[allow(dead_code)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        metadata.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn remove_relative_file(root: &Path, relative: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let parts: Vec<_> = relative.components().collect();
        let Some(Component::Normal(last)) = parts.last() else {
            return Err(anyhow!("empty unsafe path"));
        };
        let directory = open_directory_fd(root)?;
        let mut current = directory;
        let mut owned = Vec::new();
        for component in &parts[..parts.len() - 1] {
            let Component::Normal(name) = component else {
                unsafe { libc::close(directory) };
                return Err(anyhow!("unsafe path"));
            };
            let name = CString::new(name.as_bytes())?;
            let next = unsafe {
                libc::openat(
                    current,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if next < 0 {
                let error = std::io::Error::last_os_error();
                for fd in owned {
                    unsafe {
                        libc::close(fd);
                    }
                }
                unsafe { libc::close(directory) };
                return Err(error.into());
            }
            owned.push(next);
            current = next;
        }
        let name = CString::new(last.as_bytes())?;
        let result = unsafe { libc::unlinkat(current, name.as_ptr(), 0) };
        let error = if result < 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        for fd in owned {
            unsafe {
                libc::close(fd);
            }
        }
        unsafe { libc::close(directory) };
        error.map_or(Ok(()), |error| Err(error.into()))
    }
    #[cfg(not(unix))]
    {
        let _ = (root, relative);
        Err(anyhow!(
            "safe descriptor-relative removal unavailable on this platform"
        ))
    }
}

#[cfg(unix)]
fn write_relative_file_at(root: &fs::File, relative: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::{
            ffi::CString,
            os::fd::{AsRawFd, FromRawFd},
            os::unix::ffi::OsStrExt,
        };
        let parts: Vec<_> = relative.components().collect();
        let Some(Component::Normal(last)) = parts.last() else {
            return Err(anyhow!("empty unsafe path"));
        };
        let mut directory = root.try_clone()?;
        for component in &parts[..parts.len() - 1] {
            let Component::Normal(name) = component else {
                return Err(anyhow!("unsafe path"));
            };
            let name = CString::new(name.as_bytes())?;
            let mut next = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if next < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) };
                if created < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                next = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
            if next < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            directory = unsafe { fs::File::from_raw_fd(next) };
        }
        let name = CString::new(last.as_bytes())?;
        let temp_name = CString::new(format!(".skillsync-tmp-{}", super::unique_stamp()))?;
        let child = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                temp_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::mode_t,
            )
        };
        if child < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { fs::File::from_raw_fd(child) };
        let result = (|| -> Result<()> {
            file.write_all(bytes)?;
            let result = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
            if result < 0 {
                return Err(std::io::Error::last_os_error()).context("set copied file mode");
            }
            file.sync_all()?;
            let result = unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temp_name.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                )
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(directory.as_raw_fd(), temp_name.as_ptr(), 0);
            }
        }
        result
    }
    #[cfg(not(unix))]
    {
        let _ = (root, relative, bytes, mode);
        Err(anyhow!(
            "safe descriptor-relative copy unavailable on this platform"
        ))
    }
}

fn write_relative_file(root: &Path, relative: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let root = open_directory_file_bound(root)?;
        write_relative_file_at(&root, relative, bytes, mode)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, relative, bytes, mode);
        Err(anyhow!(
            "safe descriptor-relative copy unavailable on this platform"
        ))
    }
}

pub(crate) fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    assert_no_symlink_path(dst, Path::new("."))?;
    fs::create_dir_all(dst)?;
    for (r, b, m) in files(src)? {
        if !safe(&r) {
            return Err(anyhow!("unsafe path"));
        }
        assert_no_symlink_path(dst, &r)?;
        write_relative_file(dst, &r, &b, m)?;
    }
    Ok(())
}
/// Complete copy used only for deletion recovery; operational names are data.
pub(crate) fn copy_complete_tree(src: &Path, dst: &Path) -> Result<()> {
    reject_reparse_point(src, "delete snapshot source")?;
    let root = open_directory_file_bound(src)?;
    assert_no_symlink_path(dst, Path::new("."))?;
    fs::create_dir_all(dst)?;
    #[cfg(unix)]
    fn walk(dir: &fs::File, rel: &Path, dst: &Path) -> Result<()> {
        use std::{ffi::CString, os::fd::AsRawFd};
        let mut ns = fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?
            .map(|e| e.map(|x| x.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        ns.sort();
        for n in ns {
            let r = rel.join(&n);
            if !safe(&r) {
                return Err(anyhow!("unsafe delete snapshot path: {}", r.display()));
            }
            let c = CString::new(n.as_encoded_bytes())?;
            let ch = open_entry_checked(dir.as_raw_fd(), &c, &r)?;
            let m = ch.metadata()?;
            if m.is_dir() {
                fs::create_dir_all(dst.join(&r))?;
                walk(&ch, &r, dst)?;
                fs::set_permissions(dst.join(&r), fs::Permissions::from_mode(metadata_mode(&m)))?
            } else if m.is_file() {
                let mut b = Vec::new();
                (&ch).read_to_end(&mut b)?;
                write_relative_file(dst, &r, &b, metadata_mode(&m))?
            } else {
                return Err(anyhow!(
                    "unsupported delete snapshot entry: {}",
                    r.display()
                ));
            }
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        walk(&root, Path::new(""), dst)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, src, dst);
        Err(anyhow!(
            "safe descriptor-relative copy unavailable on this platform"
        ))
    }
}
pub(crate) fn copy_existing_tree(src: &Path, dst: &Path) -> Result<()> {
    reject_reparse_point(src, "destination tree root")?;
    let root = open_directory_file_bound(src)?;
    assert_no_symlink_path(dst, Path::new("."))?;
    fs::create_dir_all(dst)?;
    #[cfg(unix)]
    fn walk(dir: &fs::File, rel: &Path, dst: &Path) -> Result<()> {
        use std::{ffi::CString, os::fd::AsRawFd};
        let mut ns = fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?
            .map(|e| e.map(|x| x.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        ns.sort();
        for n in ns {
            let r = rel.join(&n);
            if operational(&r)
                && r.components()
                    .any(|c| matches!(c,Component::Normal(x) if x==".git"))
            {
                continue;
            }
            let c = CString::new(n.as_encoded_bytes())?;
            let ch = open_entry_checked(dir.as_raw_fd(), &c, &r)?;
            let m = ch.metadata()?;
            if m.is_dir() {
                fs::create_dir_all(dst.join(&r))?;
                walk(&ch, &r, dst)?
            } else if m.is_file() {
                let mut b = Vec::new();
                (&ch).read_to_end(&mut b)?;
                write_relative_file(dst, &r, &b, metadata_mode(&m))?
            } else {
                return Err(anyhow!("unsupported tree entry: {}", r.display()));
            }
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        walk(&root, Path::new(""), dst)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, src, dst);
        Err(anyhow!(
            "safe descriptor-relative copy unavailable on this platform"
        ))
    }
}
#[allow(dead_code)]
pub(crate) fn snapshot(a: &App, skill: &str, src: &Path) -> Result<(PathBuf, String)> {
    let (path, hash, replacement) = snapshot_transaction(a, skill, src)?;
    replacement.commit()?;
    Ok((path, hash))
}

pub(crate) fn snapshot_transaction(
    a: &App,
    skill: &str,
    src: &Path,
) -> Result<(PathBuf, String, Replacement)> {
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
    let baseline_parent = open_directory_file_bound(p.parent().unwrap_or(Path::new(".")))?;
    let staging_parent = tempfile::tempdir_in(p.parent().unwrap_or(Path::new(".")))?;
    let staged = staging_parent.path().join("snapshot");
    copy_tree(src, &staged)?;
    let hash = hash_dir(&staged)?;
    let replacement = replace_dir_bound(&p, &staged, &baseline_parent)?;
    Ok((p, hash, replacement))
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
fn rename_staged_dir_at(
    src: &Path,
    dst: &Path,
    destination_parent: &fs::File,
    source_parent: &fs::File,
) -> Result<()> {
    use std::{ffi::CString, os::fd::AsRawFd, os::unix::ffi::OsStrExt};
    let source = CString::new(
        src.file_name()
            .ok_or_else(|| anyhow!("source has no name"))?
            .as_bytes(),
    )?;
    let name = CString::new(
        dst.file_name()
            .ok_or_else(|| anyhow!("destination has no name"))?
            .as_bytes(),
    )?;
    let rc = unsafe {
        libc::renameat(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            name.as_ptr(),
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

pub(crate) struct Replacement {
    committed: bool,
    #[cfg(unix)]
    installed_identity: Option<(u64, u64)>,
    #[cfg(unix)]
    backup_identity: Option<(u64, u64)>,
    #[cfg(target_os = "linux")]
    installed_parent: Option<fs::File>,
    #[cfg(target_os = "linux")]
    installed_name: Option<std::ffi::OsString>,
    #[cfg(target_os = "linux")]
    backup_parent: Option<fs::File>,
    #[cfg(target_os = "linux")]
    backup_name: Option<std::ffi::OsString>,
}

impl Replacement {
    /// Finish the owning transaction.  Backups are intentionally retained:
    /// pathname deletion cannot be made safe against an external rename/race.
    pub(crate) fn commit(mut self) -> Result<()> {
        self.committed = true;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn remove_owned_dir(
    parent: &fs::File,
    name: &std::ffi::OsStr,
    identity: Option<(u64, u64)>,
) -> Result<()> {
    use std::{
        ffi::CString,
        os::fd::{AsRawFd, FromRawFd},
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
    };
    let expected = identity.ok_or_else(|| anyhow!("installed directory identity unavailable"))?;
    let c_name = CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
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
            "installed directory identity changed; retaining artifact"
        ));
    }

    fn empty(dir: &fs::File) -> Result<()> {
        use std::os::fd::AsRawFd;
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            let entry = entry?;
            let name = entry.file_name();
            let c = CString::new(name.as_encoded_bytes())?;
            let child_fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if child_fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let child = unsafe { fs::File::from_raw_fd(child_fd) };
            let child_meta = child.metadata()?;
            let expected = (child_meta.dev(), child_meta.ino());
            if child_meta.is_dir() {
                empty(&child)?;
            } else if !child_meta.is_file() {
                return Err(anyhow!(
                    "installed directory contains unsupported entry; retaining artifact"
                ));
            }
            let check_fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    c.as_ptr(),
                    libc::O_RDONLY
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW
                        | if child_meta.is_dir() {
                            libc::O_DIRECTORY
                        } else {
                            0
                        },
                )
            };
            if check_fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let check = unsafe { fs::File::from_raw_fd(check_fd) };
            let check_meta = check.metadata()?;
            if (check_meta.dev(), check_meta.ino()) != expected
                || check_meta.is_dir() != child_meta.is_dir()
                || (!child_meta.is_dir() && !check_meta.is_file())
            {
                return Err(anyhow!(
                    "installed child identity changed; retaining artifact"
                ));
            }
            let flags = if child_meta.is_dir() {
                libc::AT_REMOVEDIR
            } else {
                0
            };
            if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(())
    }
    empty(&target)?;

    // Re-open the name and compare identity immediately before unlinking it.
    // If the entry was replaced, fail closed and leave both objects intact.
    let check_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if check_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let check = unsafe { fs::File::from_raw_fd(check_fd) };
    let check_meta = check.metadata()?;
    if (check_meta.dev(), check_meta.ino()) != expected {
        return Err(anyhow!(
            "installed directory identity changed; retaining artifact"
        ));
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[allow(clippy::needless_return)]
impl Drop for Replacement {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        #[cfg(target_os = "linux")]
        if let (Some(parent), Some(live_name), Some(backup_name)) =
            (&self.backup_parent, &self.installed_name, &self.backup_name)
        {
            use std::os::fd::AsRawFd;
            use std::os::fd::FromRawFd;
            use std::os::unix::ffi::OsStrExt;
            use std::os::unix::fs::MetadataExt;
            let live = std::ffi::CString::new(live_name.as_bytes());
            let backup = std::ffi::CString::new(backup_name.as_bytes());
            let (Ok(live), Ok(backup)) = (live, backup) else {
                return;
            };
            let live_fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    live.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            let backup_fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    backup.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if live_fd < 0 || backup_fd < 0 {
                if live_fd >= 0 {
                    unsafe {
                        libc::close(live_fd);
                    }
                }
                if backup_fd >= 0 {
                    unsafe {
                        libc::close(backup_fd);
                    }
                }
                return;
            }
            let live_file = unsafe { fs::File::from_raw_fd(live_fd) };
            let backup_file = unsafe { fs::File::from_raw_fd(backup_fd) };
            let live_meta = live_file.metadata();
            let backup_meta = backup_file.metadata();
            let identities_match = live_meta.as_ref().ok().map(|m| (m.dev(), m.ino()))
                == self.installed_identity
                && backup_meta.as_ref().ok().map(|m| (m.dev(), m.ino())) == self.backup_identity;
            if !identities_match {
                return;
            }
            let status = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    parent.as_raw_fd(),
                    live.as_ptr(),
                    parent.as_raw_fd(),
                    backup.as_ptr(),
                    libc::RENAME_EXCHANGE,
                )
            };
            if status < 0 {
                return;
            }
            return;
        }
        #[cfg(target_os = "linux")]
        if self.backup_name.is_none() {
            if let (Some(parent), Some(name)) = (&self.installed_parent, &self.installed_name) {
                let _ = remove_owned_dir(parent, name, self.installed_identity);
            }
        }
        // Non-Linux replacement is rejected before a guard can be created.
    }
}

#[allow(dead_code)]
pub(crate) fn replace_dir(dst: &Path, src: &Path) -> Result<Replacement> {
    #[cfg(unix)]
    let parent = dst
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    #[cfg(unix)]
    let parent_file = open_directory_file_bound(parent)?;
    #[cfg(unix)]
    return replace_dir_bound(dst, src, &parent_file);
    #[cfg(not(unix))]
    {
        let _ = (dst, src);
        Err(anyhow!(
            "identity-safe replacement unavailable on this platform"
        ))
    }
}

#[cfg(unix)]
pub(crate) fn replace_dir_bound(
    dst: &Path,
    src: &Path,
    destination_parent: &fs::File,
) -> Result<Replacement> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    verify_bound_parent(dst, destination_parent)?;
    let destination_name = dst
        .file_name()
        .ok_or_else(|| anyhow!("destination has no name"))?
        .to_os_string();
    let destination_c = CString::new(destination_name.as_bytes())?;
    let source_parent = open_directory_file_bound(
        src.parent()
            .ok_or_else(|| anyhow!("source has no parent"))?,
    )?;
    let existing = destination_directory_exists(destination_parent, &destination_c, dst)?;
    if !existing {
        rename_staged_dir_at(src, dst, destination_parent, &source_parent)?;
        let installed_identity = entry_identity_at(destination_parent, &destination_c, dst)?;
        let replacement = Replacement {
            committed: false,
            #[cfg(unix)]
            installed_identity: Some(installed_identity),
            #[cfg(unix)]
            backup_identity: None,
            #[cfg(target_os = "linux")]
            installed_parent: Some(destination_parent.try_clone()?),
            #[cfg(target_os = "linux")]
            installed_name: Some(destination_name.clone()),
            #[cfg(target_os = "linux")]
            backup_parent: None,
            #[cfg(target_os = "linux")]
            backup_name: None,
        };
        verify_bound_parent(dst, destination_parent)?;
        return Ok(replacement);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let dp = destination_parent.try_clone()?;
        let sp = source_parent.try_clone()?;
        let dp_fd = dp.as_raw_fd();
        let sp_fd = sp.as_raw_fd();
        let dn = CString::new(
            dst.file_name()
                .ok_or_else(|| anyhow!("destination has no name"))?
                .as_bytes(),
        )?;
        let sn = CString::new(
            src.file_name()
                .ok_or_else(|| anyhow!("source has no name"))?
                .as_bytes(),
        )?;
        let status = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                dp_fd,
                dn.as_ptr(),
                sp_fd,
                sn.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        let result = if status < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            let backup_name = format!(".skillsync-replaced-{}", unique_stamp());
            let backup = dst.parent().unwrap().join(&backup_name);
            let backup_c = CString::new(backup_name.as_bytes())?;
            let moved = unsafe { libc::renameat(sp_fd, sn.as_ptr(), dp_fd, backup_c.as_ptr()) };
            if moved < 0 {
                let backup_error = std::io::Error::last_os_error();
                let restored = unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        dp_fd,
                        dn.as_ptr(),
                        sp_fd,
                        sn.as_ptr(),
                        libc::RENAME_EXCHANGE,
                    )
                };
                if restored < 0 {
                    // The exchange left the old object at `src`.  It must not
                    // remain owned solely by a caller's TempDir.  First move
                    // it to a durable sibling; copying is the last resort and
                    // deliberately retains both objects rather than deleting
                    // an object through a raceable path.
                    let recovery = dst
                        .parent()
                        .unwrap()
                        .join(format!(".skillsync-recovery-{}", unique_stamp()));
                    let recovery_c = CString::new(recovery.file_name().unwrap().as_bytes())?;
                    let moved_recovery =
                        unsafe { libc::renameat(sp_fd, sn.as_ptr(), dp_fd, recovery_c.as_ptr()) };
                    if moved_recovery == 0 {
                        return Err(anyhow!(
                            "directory replacement backup failed: {backup_error}; rollback failed: {}; recovery required; displaced old destination retained at {}",
                            std::io::Error::last_os_error(), recovery.display()
                        ));
                    }
                    let copy_error = copy_complete_tree(src, &recovery);
                    return Err(match copy_error {
                        Ok(()) => anyhow!(
                            "directory replacement backup failed: {backup_error}; rollback failed; recovery required; displaced old destination copied to {}",
                            recovery.display()
                        ),
                        Err(copy_error) => anyhow!(
                            "directory replacement backup failed: {backup_error}; rollback failed; recovery required; durable recovery copy {} failed: {copy_error}",
                            recovery.display()
                        ),
                    });
                }
                return Err(anyhow!(
                    "directory replacement backup failed; original destination restored: {backup_error}"
                ));
            }
            let installed_identity = entry_identity_at(&dp, &dn, dst)?;
            let backup_identity = entry_identity_at(&dp, &backup_c, &backup)?;
            let replacement = Replacement {
                committed: false,
                #[cfg(unix)]
                installed_identity: Some(installed_identity),
                #[cfg(unix)]
                backup_identity: Some(backup_identity),
                #[cfg(target_os = "linux")]
                installed_parent: Some(dp.try_clone()?),
                #[cfg(target_os = "linux")]
                installed_name: Some(destination_name.clone()),
                backup_parent: Some(dp.try_clone()?),
                backup_name: Some(backup_name.into()),
            };
            verify_bound_parent(dst, destination_parent)?;
            Ok(replacement)
        };
        result
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = src;
        Err(anyhow!(
            "identity-safe replacement of an existing destination is unavailable on this platform"
        ))
    }
}
#[cfg(not(unix))]
pub(crate) fn replace_dir_bound(
    _dst: &Path,
    _src: &Path,
    _parent: &fs::File,
) -> Result<Replacement> {
    Err(anyhow!(
        "identity-safe replacement unavailable on this platform"
    ))
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
                remove_relative_file(dst, &path)?;
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
        write_relative_file(dst, &path, &bytes, mode)?;
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
