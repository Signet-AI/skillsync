use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(name = "skillsync", version)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}
#[derive(Subcommand)]
enum Cmd {
    Init {
        #[arg(long)]
        library: Option<PathBuf>,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCmd,
    },
    Subscribe {
        repository: Option<String>,
        #[arg(long)]
        skill: Option<String>,
    },
    Publish {
        skill: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        dry_run: bool,
    },
    Update,
    Sync,
    Worker {
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 300)]
        interval: u64,
    },
    Status,
    Diff,
    Doctor,
    Unsubscribe {
        skill: String,
    },
    Unpublish {
        skill: String,
        #[arg(long)]
        repo: String,
    },
    Set {
        #[command(subcommand)]
        command: SetCmd,
    },
    Harness {
        #[command(subcommand)]
        command: HarnessCmd,
    },
}
#[derive(Subcommand)]
enum HarnessCmd {
    Link {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        skill: String,
    },
    Unlink {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        skill: String,
    },
    List,
}
#[derive(Subcommand)]
enum SetCmd {
    Create { name: String },
    List,
    Show { name: String },
    Add { name: String, skill: String },
    Remove { name: String, skill: String },
}
#[derive(Subcommand)]
enum ConfigCmd {
    Path,
    Edit,
}
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct State {
    version: u32,
    library: String,
    subscriptions: BTreeMap<String, Subscription>,
    publications: BTreeMap<String, Publication>,
    #[serde(default)]
    pending_publications: BTreeMap<String, PendingPublication>,
    #[serde(default)]
    sets: BTreeMap<String, SkillSet>,
    #[serde(default)]
    harness_links: BTreeMap<String, HarnessLink>,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
struct HarnessLink {
    skill: String,
    harness_root: String,
    canonical_path: String,
    link_path: String,
    status: String,
}
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct SkillSet {
    #[serde(default)]
    members: BTreeSet<String>,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Subscription {
    skill: String,
    source: String,
    branch: String,
    source_path: String,
    baseline_path: String,
    baseline_hash: String,
    local_path: String,
    status: String,
    recovery_path: Option<String>,
    last_sync: u64,
    update_count: u64,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Publication {
    skill: String,
    destination: String,
    branch: String,
    path: String,
    approved: bool,
    status: String,
    last_hash: Option<String>,
    last_sync: u64,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
struct PendingPublication {
    publication: Publication,
}
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct FileConfig {
    library: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileData {
    bytes: Vec<u8>,
    mode: u32,
}
struct App {
    config: PathBuf,
    library: PathBuf,
    state_path: PathBuf,
    baselines: PathBuf,
    recovery: PathBuf,
    state: State,
}

static WORKER_STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn open_directory_fd(path: &Path) -> Result<std::os::fd::RawFd> {
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
fn open_child_file(config: &Path, name: &str, create: bool) -> Result<fs::File> {
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
fn open_child_file(config: &Path, name: &str, create: bool) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(create);
    Ok(options.open(config.join(name))?)
}

fn open_advisory_lock(config: &Path, name: &str, create: bool) -> Result<fs::File> {
    open_child_file(config, name, create)
}

fn acquire_named_lock(config: &Path, name: &str, busy_message: &str) -> Result<fs::File> {
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

struct StateLock {
    file: fs::File,
}

impl StateLock {
    fn acquire(config: &Path) -> Result<Self> {
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

struct WorkerLease {
    file: fs::File,
}

impl WorkerLease {
    fn acquire(config: &Path) -> Result<Self> {
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

fn worker_status(config: &Path) -> Result<&'static str> {
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
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok("running"),
        Err(error) => Err(error).context("inspect worker status"),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn unique_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
fn config_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("SKILLSYNC_CONFIG_DIR") {
        return x.into();
    }
    if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .map(|x| PathBuf::from(x).join("skillsync/config"))
            .unwrap_or_else(|| PathBuf::from(".skillsync/config"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|x| PathBuf::from(x).join("Library/Application Support/skillsync"))
            .unwrap_or_else(|| PathBuf::from(".skillsync"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(|x| PathBuf::from(x).join("skillsync"))
            .or_else(|| {
                std::env::var_os("HOME").map(|x| PathBuf::from(x).join(".config/skillsync"))
            })
            .unwrap_or_else(|| PathBuf::from(".skillsync"))
    }
}
fn default_library() -> PathBuf {
    std::env::var_os("SKILLSYNC_LIBRARY")
        .map(Into::into)
        .or_else(|| {
            std::env::var_os(if cfg!(target_os = "windows") {
                "USERPROFILE"
            } else {
                "HOME"
            })
            .map(|x| PathBuf::from(x).join(".agents/skills"))
        })
        .unwrap_or_else(|| PathBuf::from(".agents/skills"))
}
fn resolve_library_path(path: &Path) -> Result<PathBuf> {
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
fn atomic(p: &Path, b: &[u8]) -> Result<()> {
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
fn atomic_unix(p: &Path, b: &[u8]) -> Result<()> {
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
fn atomic_portable(p: &Path, b: &[u8]) -> Result<()> {
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
impl App {
    fn load() -> Result<Self> {
        let c = config_dir();
        assert_no_symlink_path(&c, Path::new("."))?;
        let sp = c.join("state.json");
        let cfg_path = c.join("config.toml");
        let config_exists = checked_regular_path(&cfg_path, "config")?;
        let file_cfg = if config_exists {
            let contents = String::from_utf8(read_regular_file(&cfg_path, None)?)
                .context("config.toml is not valid UTF-8")?;
            toml::from_str::<FileConfig>(&contents).context("invalid config.toml")?
        } else {
            FileConfig::default()
        };
        let configured_library = file_cfg.library.map(PathBuf::from);
        let requested_library = std::env::var_os("SKILLSYNC_LIBRARY")
            .map(PathBuf::from)
            .or(configured_library)
            .unwrap_or_else(default_library);
        let expected_library = resolve_library_path(&requested_library)?;
        let state_exists = checked_regular_path(&sp, "state")?;
        let mut state: State = if state_exists {
            serde_json::from_slice(&read_regular_file(&sp, None)?)?
        } else {
            State {
                version: 4,
                library: expected_library.display().to_string(),
                ..Default::default()
            }
        };
        if state.version > 4 {
            return Err(anyhow!("unsupported state version: {}", state.version));
        }
        if state.version < 3 {
            let old_publications = std::mem::take(&mut state.publications);
            state.publications = old_publications
                .into_values()
                .map(|publication| {
                    let key = publication_key(
                        &publication.skill,
                        &publication.destination,
                        &publication.branch,
                        &publication.path,
                    );
                    (key, publication)
                })
                .collect();
            state.version = 3;
        }
        if state.version < 4 {
            state.version = 4;
        }
        validate_set_state(&state)?;
        let library = if state_exists && !state.library.is_empty() {
            let persisted = PathBuf::from(&state.library);
            if persisted != expected_library {
                return Err(anyhow!(
                    "persisted library does not match configured library"
                ));
            }
            expected_library.clone()
        } else {
            expected_library.clone()
        };
        assert_no_symlink_path(&library, Path::new("."))?;
        validate_harness_links(&state, &library)?;
        let baselines = c.join("baselines");
        let recovery = c.join("recovery");
        Ok(Self {
            config: c,
            library,
            state_path: sp,
            baselines,
            recovery,
            state,
        })
    }
    fn save(&self) -> Result<()> {
        atomic(&self.state_path, &serde_json::to_vec_pretty(&self.state)?)
    }
}
fn safe(p: &Path) -> bool {
    p.is_relative()
        && !p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}
fn strict_component(s: &str, what: &str) -> Result<String> {
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
fn windows_reserved_component(s: &str) -> bool {
    let base = s.split('.').next().unwrap_or(s);
    let upper = base.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
}
fn set_member_id(skill: &str) -> Result<String> {
    Ok(format!(
        "library:{}",
        strict_component(skill, "skill name")?
    ))
}
fn set_member_name(id: &str) -> Result<String> {
    let name = id
        .strip_prefix("library:")
        .ok_or_else(|| anyhow!("invalid set member identity"))?;
    strict_component(name, "set member skill name")
}
fn validate_set_state(state: &State) -> Result<()> {
    for (name, set) in &state.sets {
        strict_component(name, "set name")?;
        for member in &set.members {
            set_member_name(member)?;
        }
    }
    Ok(())
}
fn source_rel(s: &str) -> Result<String> {
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
fn relationship_key(source: &str, source_path: &str) -> String {
    let mut h = Sha256::new();
    h.update(source.as_bytes());
    h.update([0]);
    h.update(source_path.as_bytes());
    format!("rel-{:x}", h.finalize())
}
fn publication_key(skill: &str, destination: &str, branch: &str, path: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"publication");
    h.update([0]);
    for value in [skill, destination, branch, path] {
        h.update(value.as_bytes());
        h.update([0]);
    }
    format!("pub-{:x}", h.finalize())
}
fn package_paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left == "."
        || right == "."
        || right.starts_with(&format!("{left}/"))
        || left.starts_with(&format!("{right}/"))
}
fn subscription_overlaps(state: &State, source: &str, source_path: &str) -> Option<String> {
    state.subscriptions.iter().find_map(|(key, sub)| {
        (sub.source == source && package_paths_overlap(&sub.source_path, source_path))
            .then(|| key.clone())
    })
}
fn operational(p: &Path) -> bool {
    p.components().any(|c|matches!(c,Component::Normal(x) if matches!(x.to_str(),Some(".git"|".cache"|"logs"|"credentials"|"private"))))||p.file_name().and_then(|x|x.to_str()).map(|x|x==".env"||x.ends_with(".pem")||x.ends_with(".key")).unwrap_or(false)
}
fn portable_rel(path: &Path) -> String {
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
fn read_regular_file(path: &Path, expected: Option<&fs::Metadata>) -> Result<Vec<u8>> {
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
fn files(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>, u32)>> {
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
fn write_file_data(path: &Path, data: &FileData) -> Result<()> {
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
fn hash_dir(p: &Path) -> Result<String> {
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
fn assert_no_symlink_path(root: &Path, rel: &Path) -> Result<()> {
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
fn reject_reparse_point(path: &Path, label: &str) -> Result<()> {
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
fn reject_reparse_point(_path: &Path, _label: &str) -> Result<()> {
    Ok(())
}
fn checked_regular_path(path: &Path, label: &str) -> Result<bool> {
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
fn validate_state_path(root: &Path, target: &Path, label: &str) -> Result<()> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| anyhow!("{label} escaped state directory"))?;
    if relative.as_os_str().is_empty() || !safe(relative) {
        return Err(anyhow!("unsafe {label} path: {}", target.display()));
    }
    assert_no_symlink_path(root, relative)
}
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
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
fn copy_existing_tree(src: &Path, dst: &Path) -> Result<()> {
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
fn discover(root: &Path) -> Result<Vec<(String, PathBuf, String)>> {
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
                    let name = manifest_name(&x)
                        .unwrap_or_else(|| rel.file_name().unwrap().to_string_lossy().into());
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
        o.push((
            manifest_name(root).unwrap_or_else(|| {
                root.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            }),
            root.to_path_buf(),
            ".".into(),
        ))
    }
    r(root, root, &mut o)?;
    o.sort_by(|a, b| a.2.cmp(&b.2));
    Ok(o)
}
fn manifest_name(p: &Path) -> Option<String> {
    let text = String::from_utf8(read_regular_file(&p.join("SKILL.md"), None).ok()?).ok()?;
    text.lines()
        .find_map(|l| {
            l.strip_prefix("name:")
                .map(|x| x.trim().trim_matches('"').to_string())
        })
        .filter(|x| !x.is_empty())
}
fn normalize(s: &str) -> Result<String> {
    if s.is_empty() || s.chars().any(|c| c.is_control()) {
        return Err(anyhow!("unsupported repository source"));
    }
    if Path::new(s).exists() {
        return Ok(fs::canonicalize(s)?.display().to_string());
    }
    if s.contains('?') || s.contains('#') {
        return Err(anyhow!("repository URL contains query or fragment"));
    }
    if s.starts_with("https://") || s.starts_with("http://") {
        let rest = s.split_once("://").map(|x| x.1).unwrap_or("");
        if rest.contains('@') {
            return Err(anyhow!("credential-bearing repository URL rejected"));
        }
        return Ok(s.into());
    }
    if s.starts_with("ssh://") {
        let authority = s
            .trim_start_matches("ssh://")
            .split('/')
            .next()
            .unwrap_or_default();
        if authority
            .split_once('@')
            .map(|(user, _)| user.contains(':'))
            .unwrap_or(false)
        {
            return Err(anyhow!("credential-bearing repository URL rejected"));
        }
        return Ok(s.into());
    }
    if s.starts_with("git@") && s.contains(':') {
        return Ok(s.into());
    }
    let shorthand = s.split('/').collect::<Vec<_>>();
    if shorthand.len() == 2
        && shorthand.iter().all(|x| {
            !x.is_empty() && *x != "." && *x != ".." && !x.contains('\\') && !x.contains(':')
        })
        && !s.starts_with('/')
    {
        return Ok(format!("https://github.com/{s}.git"));
    }
    Err(anyhow!("unsupported repository: {s}"))
}
fn validate_branch(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.chars().any(|character| character.is_control())
        || branch.contains('\\')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.contains("//")
        || branch.contains("..")
        || branch.contains("@{")
    {
        return Err(anyhow!("unsafe Git branch: {branch}"));
    }
    Ok(())
}
fn terminate_git_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as libc::pid_t);
        unsafe {
            libc::kill(process_group, libc::SIGTERM);
        }
        thread::sleep(Duration::from_millis(25));
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    if let Some(path) = cwd {
        command.current_dir(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("git is not installed")?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_git_process(&mut child);
            return Err(anyhow!("git stdout pipe unavailable"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_git_process(&mut child);
            return Err(anyhow!("git stderr pipe unavailable"));
        }
    };
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stdout;
        let _ = stream.read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stream = stderr;
        let _ = stream.read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) {
            terminate_git_process(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(anyhow!("worker cancelled during git operation"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                terminate_git_process(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(error).context("wait for git operation");
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow!("git stdout reader panicked"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("git stderr reader panicked"))?;
    if !status.success() {
        let error = String::from_utf8_lossy(&stderr).replace('\n', " ");
        return Err(anyhow!("git operation failed: {}", error.trim()));
    }
    Ok(String::from_utf8_lossy(&stdout).trim().into())
}
fn branch(repo: &str) -> Result<String> {
    if let Some(b) = run_git(None, &["ls-remote", "--symref", repo, "HEAD"])
        .ok()
        .and_then(|x| {
            x.lines().find_map(|l| {
                l.strip_prefix("ref: refs/heads/")
                    .and_then(|x| x.split_whitespace().next())
                    .map(str::to_string)
            })
        })
    {
        return Ok(b);
    }
    let refs = run_git(None, &["ls-remote", "--heads", repo])?;
    let mut branches = refs
        .lines()
        .filter_map(|l| l.split_once("refs/heads/").map(|x| x.1.to_string()))
        .collect::<Vec<_>>();
    branches.sort();
    branches
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("remote has no branches"))
}
fn clone_repo(repo: &str) -> Result<(tempfile::TempDir, String)> {
    let t = tempfile::tempdir()?;
    let b = branch(repo)?;
    validate_branch(&b)?;
    run_git(
        Some(t.path()),
        &["clone", "--quiet", "--no-checkout", repo, "."],
    )?;
    run_git(
        Some(t.path()),
        &["checkout", "--quiet", "-B", &b, &format!("origin/{b}")],
    )?;
    Ok((t, b))
}
fn clone_repo_branch(repo: &str, requested: Option<&str>) -> Result<(tempfile::TempDir, String)> {
    let (t, default) = clone_repo(repo)?;
    if let Some(b) = requested {
        validate_branch(b)?;
        if run_git(
            Some(t.path()),
            &["show-ref", "--verify", &format!("refs/remotes/origin/{b}")],
        )
        .is_err()
        {
            return Err(anyhow!("requested branch does not exist: {b}"));
        }
        run_git(
            Some(t.path()),
            &["checkout", "--quiet", "-B", b, &format!("origin/{b}")],
        )?;
        return Ok((t, b.to_owned()));
    }
    Ok((t, default))
}
fn find_skill(root: &Path, q: &str) -> Result<(String, PathBuf, String)> {
    let all = discover(root)?;
    let matches = if q.contains('/') {
        all.into_iter()
            .filter(|(_, _, p)| p == q)
            .collect::<Vec<_>>()
    } else {
        all.into_iter()
            .filter(|(n, _, _)| n == q)
            .collect::<Vec<_>>()
    };
    match matches.as_slice() {
        [] => Err(anyhow!("skill not found: {q}")),
        [one] => Ok(one.clone()),
        many => Err(anyhow!(
            "skill selection is ambiguous: {}; candidates: {}",
            q,
            many.iter()
                .map(|(_, _, p)| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}
fn skill_query(s: &str) -> Result<String> {
    if s.contains('/') || s.contains('\\') {
        let p = s.replace('\\', "/");
        source_rel(&p)
    } else {
        strict_component(s, "skill name")
    }
}
fn envelope(json: bool, ok: bool, msg: &str, extra: serde_json::Value) {
    let mut m = serde_json::Map::new();
    m.insert("ok".into(), ok.into());
    m.insert("message".into(), msg.into());
    if let serde_json::Value::Object(x) = extra {
        m.extend(x)
    }
    if json {
        println!("{}", serde_json::Value::Object(m))
    } else {
        println!("{msg}")
    }
}
fn snapshot(a: &App, skill: &str, src: &Path) -> Result<(PathBuf, String)> {
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
#[cfg(unix)]
fn rename_staged_dir(src: &Path, dst: &Path) -> Result<()> {
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
fn rename_staged_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::rename(src, dst)?;
    Ok(())
}
fn replace_dir(dst: &Path, src: &Path) -> Result<()> {
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
fn sync_managed_tree(src: &Path, dst: &Path) -> Result<Vec<PathBuf>> {
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
fn cached_changes(cwd: &Path) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["diff", "--cached", "--quiet"])
        .output()
        .context("git is not installed")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(anyhow!(
            "git staged diff check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}
fn init_empty_destination(repo: &str) -> Result<(tempfile::TempDir, String)> {
    let t = tempfile::tempdir()?;
    run_git(Some(t.path()), &["init", "--quiet"])?;
    run_git(Some(t.path()), &["checkout", "--quiet", "-b", "main"])?;
    run_git(Some(t.path()), &["remote", "add", "origin", repo])?;
    Ok((t, "main".into()))
}
fn type_collision(local: &BTreeSet<PathBuf>, upstream: &BTreeSet<PathBuf>) -> bool {
    local.iter().any(|local_path| {
        upstream.iter().any(|upstream_path| {
            local_path != upstream_path
                && (local_path.starts_with(upstream_path) || upstream_path.starts_with(local_path))
        })
    })
}
fn merge_tree(base: &Path, local: &Path, up: &Path, out: &Path) -> Result<bool> {
    let local_paths = files(local)?
        .into_iter()
        .map(|(path, _, _)| path)
        .collect::<BTreeSet<_>>();
    let upstream_paths = files(up)?
        .into_iter()
        .map(|(path, _, _)| path)
        .collect::<BTreeSet<_>>();
    if type_collision(&local_paths, &upstream_paths) {
        return Ok(true);
    }
    copy_tree(local, out)?;
    let mut paths =
        BTreeMap::<PathBuf, (Option<FileData>, Option<FileData>, Option<FileData>)>::new();
    for root in [base, local, up] {
        for (r, bytes, mode) in files(root)? {
            let entry = paths.entry(r).or_insert((None, None, None));
            let data = FileData { bytes, mode };
            if root == base {
                entry.0 = Some(data)
            } else if root == local {
                entry.1 = Some(data)
            } else {
                entry.2 = Some(data)
            }
        }
    }
    let mut conflict = false;
    for (r, (b, l, u)) in paths {
        if l == u {
            continue;
        }
        if l == b {
            if let Some(data) = u {
                write_file_data(&out.join(&r), &data)?;
            } else {
                let _ = fs::remove_file(out.join(&r));
            }
        } else if u == b {
            continue;
        } else if let (Some(base_data), Some(local_data), Some(upstream_data)) = (b, l, u) {
            if local_data.mode != base_data.mode
                && upstream_data.mode != base_data.mode
                && local_data.mode != upstream_data.mode
            {
                conflict = true;
                continue;
            }
            let td = tempfile::tempdir()?;
            let base_file = td.path().join("base");
            let local_file = td.path().join("local");
            let upstream_file = td.path().join("upstream");
            write_file_data(&base_file, &base_data)?;
            write_file_data(&local_file, &local_data)?;
            write_file_data(&upstream_file, &upstream_data)?;
            let output = Command::new("git")
                .args([
                    "merge-file",
                    "-p",
                    local_file.to_str().unwrap(),
                    base_file.to_str().unwrap(),
                    upstream_file.to_str().unwrap(),
                ])
                .output()?;
            if output.status.code() == Some(0) {
                let mode = if local_data.mode == base_data.mode {
                    upstream_data.mode
                } else {
                    local_data.mode
                };
                write_file_data(
                    &out.join(&r),
                    &FileData {
                        bytes: output.stdout,
                        mode,
                    },
                )?;
            } else {
                conflict = true
            }
        } else {
            conflict = true
        }
    }
    Ok(conflict)
}
fn update_one(a: &mut App, key: &str) -> Result<serde_json::Value> {
    let mut s = a
        .state
        .subscriptions
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow!("subscription not found"))?;
    strict_component(&s.skill, "subscription skill name")?;
    strict_component(key, "subscription relationship key")?;
    let local = PathBuf::from(&s.local_path);
    let expected_local = a.library.join(&s.skill);
    if local != expected_local {
        return Err(anyhow!("subscription local path does not match library"));
    }
    let local_relative = local
        .strip_prefix(&a.library)
        .map_err(|_| anyhow!("subscription destination escaped library"))?;
    if !safe(local_relative) {
        return Err(anyhow!("subscription destination escaped library"));
    }
    assert_no_symlink_path(&a.library, local_relative)?;
    let _ = source_rel(&s.source_path)?;
    let base = PathBuf::from(&s.baseline_path);
    let expected_base = a.baselines.join(key);
    if base != expected_base {
        return Err(anyhow!(
            "subscription baseline path does not match state directory"
        ));
    }
    validate_state_path(&a.baselines, &base, "baseline")?;
    if let Some(recovery) = &s.recovery_path {
        validate_state_path(&a.recovery, Path::new(recovery), "recovery")?;
    }
    let (repo, b) = match clone_repo_branch(&s.source, Some(&s.branch)) {
        Ok(x) => x,
        Err(e) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(e),
        Err(e) => {
            s.status = if e.to_string().to_lowercase().contains("auth") {
                "authentication_required".into()
            } else {
                "offline".into()
            };
            let status = s.status.clone();
            let display_skill = s.skill.clone();
            a.state.subscriptions.insert(key.into(), s);
            a.save()?;
            return Ok(
                serde_json::json!({"skill":display_skill,"relationship":key,"status":status}),
            );
        }
    };
    s.branch = b.clone();
    let (_, up, _) = find_skill(repo.path(), &s.source_path)?;
    let base = PathBuf::from(&s.baseline_path);
    let lh = hash_dir(&local)?;
    if lh != s.baseline_hash {
        s.status = "customized".into()
    }
    let uh = hash_dir(&up)?;
    if uh == s.baseline_hash {
        s.status = if lh == s.baseline_hash {
            "synced".into()
        } else {
            "customized".into()
        };
        a.state.subscriptions.insert(key.into(), s.clone());
        a.save()?;
        return Ok(serde_json::json!({"skill":s.skill,"relationship":key,"status":s.status}));
    }
    let stage_parent = local
        .parent()
        .ok_or_else(|| anyhow!("subscription path has no parent"))?;
    fs::create_dir_all(stage_parent)?;
    let stage = tempfile::tempdir_in(stage_parent)?;
    let conflict = merge_tree(&base, &local, &up, stage.path())?;
    if conflict {
        assert_no_symlink_path(&a.recovery, Path::new("."))?;
        fs::create_dir_all(&a.recovery)?;
        assert_no_symlink_path(&a.recovery, Path::new("."))?;
        let rec = a.recovery.join(format!("{}-{}", key, unique_stamp()));
        validate_state_path(&a.recovery, &rec, "recovery")?;
        fs::create_dir_all(&rec)?;
        copy_tree(&local, &rec.join("local"))?;
        copy_tree(&up, &rec.join("incoming"))?;
        s.recovery_path = Some(rec.display().to_string());
        s.status = "conflict".into()
    } else if hash_dir(&local)? != lh {
        s.status = "changed_during_update".into()
    } else {
        replace_dir(&local, stage.path())?;
        let (bp, h) = snapshot(a, key, &up)?;
        s.baseline_path = bp.display().to_string();
        s.baseline_hash = h;
        s.status = "synced".into();
        s.update_count += 1;
        s.last_sync = now()
    }
    a.state.subscriptions.insert(key.into(), s.clone());
    a.save()?;
    Ok(serde_json::json!({"skill":s.skill,"relationship":key,"status":s.status}))
}
fn publish_to_repo(
    a: &mut App,
    skill: &str,
    url: &str,
    previous: Option<&Publication>,
) -> Result<serde_json::Value> {
    strict_component(skill, "skill name")?;
    let _ = normalize(url)?;
    let source = a.library.join(skill);
    let source_relative = source
        .strip_prefix(&a.library)
        .map_err(|_| anyhow!("publication source escaped library"))?;
    if !safe(source_relative) {
        return Err(anyhow!("publication source escaped library"));
    }
    assert_no_symlink_path(&a.library, source_relative)?;
    if !source.is_dir() {
        return Err(anyhow!("skill not found in library"));
    }
    let source_candidate_parent = tempfile::tempdir()?;
    let source_candidate = source_candidate_parent.path().join("skill");
    copy_tree(&source, &source_candidate)?;
    let current_hash = hash_dir(&source_candidate)?;
    if hash_dir(&source)? != current_hash {
        return Err(anyhow!("source changed during publication; retry"));
    }
    let source_files = files(&source_candidate)?;
    let requested_branch = previous.map(|publication| publication.branch.as_str());
    let (tmp, branch_name) = match clone_repo_branch(url, requested_branch) {
        Ok(value) => value,
        Err(error) if error.to_string().contains("remote has no branches") => {
            init_empty_destination(url)?
        }
        Err(error) => return Err(error),
    };
    let destination_rel = format!("skills/{skill}");
    let destination_rel_path = Path::new(&destination_rel);
    assert_no_symlink_path(tmp.path(), destination_rel_path)?;
    let destination = tmp.path().join(destination_rel_path);
    let destination_exists = destination.exists();
    if let Ok(metadata) = fs::symlink_metadata(&destination) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(anyhow!("destination skill path is not a regular directory"));
        }
    }
    let existing_hash = if destination_exists {
        hash_dir(&destination)?
    } else {
        String::new()
    };
    let existing_is_only_operational = destination_exists && files(&destination)?.is_empty();
    let previously_published = previous
        .and_then(|publication| publication.last_hash.as_ref())
        .is_some_and(|hash| hash == &existing_hash);
    if destination_exists
        && existing_hash != current_hash
        && !existing_is_only_operational
        && !previously_published
    {
        return Err(anyhow!("destination has unexplained modifications"));
    }
    let destination_stage_parent = tempfile::tempdir_in(tmp.path())?;
    let staged_destination = destination_stage_parent.path().join("skill");
    if destination_exists {
        copy_existing_tree(&destination, &staged_destination)?;
    } else {
        fs::create_dir_all(&staged_destination)?;
    }
    let removed = sync_managed_tree(&source_candidate, &staged_destination)?;
    if let Some(parent) = destination.parent() {
        assert_no_symlink_path(tmp.path(), parent.strip_prefix(tmp.path())?)?;
        fs::create_dir_all(parent)?;
    }
    replace_dir(&destination, &staged_destination)?;
    for (path, _, _) in &source_files {
        let relative = format!("{destination_rel}/{}", path.to_string_lossy());
        run_git(Some(tmp.path()), &["add", "--", &relative])?;
    }
    for path in &removed {
        let relative = format!("{destination_rel}/{}", path.to_string_lossy());
        run_git(Some(tmp.path()), &["add", "-u", "--", &relative])?;
    }
    if hash_dir(&source)? != current_hash {
        return Err(anyhow!("source changed during publication; retry"));
    }
    let changed = cached_changes(tmp.path())?;
    let key = publication_key(skill, url, &branch_name, &destination_rel);
    if changed {
        run_git(
            Some(tmp.path()),
            &["commit", "-m", &format!("Update skill {skill}")],
        )?;
        if hash_dir(&source)? != current_hash {
            return Err(anyhow!("source changed during publication; retry"));
        }
        let pending = Publication {
            skill: skill.to_owned(),
            destination: url.to_owned(),
            branch: branch_name.clone(),
            path: destination_rel.clone(),
            approved: true,
            status: "pending_push".into(),
            last_hash: Some(current_hash.clone()),
            last_sync: now(),
        };
        a.state.pending_publications.insert(
            key.clone(),
            PendingPublication {
                publication: pending,
            },
        );
        a.save().context("persist publication intent before push")?;
        run_git(Some(tmp.path()), &["push", "origin", &branch_name])?;
    }
    if hash_dir(&destination)? != current_hash {
        return Err(anyhow!("published destination did not match source scope"));
    }
    let remote_ref = run_git(
        None,
        &["ls-remote", url, &format!("refs/heads/{branch_name}")],
    )?;
    let remote_hash = remote_ref
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("published branch was not visible on the remote"))?;
    let local_hash = run_git(Some(tmp.path()), &["rev-parse", "HEAD"])?;
    if remote_hash != local_hash {
        return Err(anyhow!("remote readback did not match published commit"));
    }
    let publication = Publication {
        skill: skill.to_owned(),
        destination: url.to_owned(),
        branch: branch_name.clone(),
        path: destination_rel.clone(),
        approved: true,
        status: "synced".into(),
        last_hash: Some(current_hash),
        last_sync: now(),
    };
    a.state.pending_publications.remove(&key);
    a.state.publications.insert(key, publication);
    a.save()?;
    Ok(serde_json::json!({"skill":skill,"status":"published"}))
}
fn status_for_error(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("auth") {
        "authentication_required"
    } else if lower.contains("offline") || lower.contains("not installed") {
        "offline"
    } else if lower.contains("permission") || lower.contains("access denied") {
        "permission_denied"
    } else {
        "conflict"
    }
}

fn sync_all(a: &mut App, continue_on_error: bool) -> Result<serde_json::Value> {
    let keys = a.state.subscriptions.keys().cloned().collect::<Vec<_>>();
    let mut results = vec![];
    for key in keys {
        match update_one(a, &key) {
            Ok(result) => results.push(result),
            Err(error) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(error),
            Err(error) if !continue_on_error => return Err(error),
            Err(error) => {
                let error_text = error.to_string();
                let status = status_for_error(&error_text);
                if let Some(subscription) = a.state.subscriptions.get_mut(&key) {
                    subscription.status = status.into();
                    subscription.last_sync = now();
                }
                a.save()?;
                results.push(serde_json::json!({
                    "relationship": key,
                    "status": status,
                    "error": error_text,
                }));
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
        .collect::<BTreeSet<_>>();
    for (key, intent) in pending {
        let previous = a.state.publications.get(&key).cloned();
        match publish_to_repo(a, &intent.skill, &intent.destination, previous.as_ref()) {
            Ok(result) => results.push(result),
            Err(error) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(error),
            Err(error) if !continue_on_error => return Err(error),
            Err(error) => {
                let error_text = error.to_string();
                let status = status_for_error(&error_text);
                if let Some(pending) = a.state.pending_publications.get_mut(&key) {
                    pending.publication.status = status.into();
                    pending.publication.last_sync = now();
                }
                if let Some(publication) = a.state.publications.get_mut(&key) {
                    publication.status = status.into();
                    publication.last_sync = now();
                }
                a.save()?;
                results.push(serde_json::json!({
                    "skill": intent.skill,
                    "relationship": key,
                    "status": status,
                    "error": error_text,
                }));
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
        match publish_to_repo(a, &skill, &destination, Some(&previous)) {
            Ok(result) => results.push(result),
            Err(error) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(error),
            Err(error) => {
                let error_text = error.to_string();
                let status = status_for_error(&error_text);
                if let Some(publication) = a.state.publications.get_mut(&key) {
                    publication.status = status.into();
                    publication.last_sync = now();
                }
                a.save()?;
                results.push(serde_json::json!({
                    "skill": skill,
                    "status": status,
                    "error": error_text,
                }));
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

fn run_worker_locked(a: &mut App, once: bool, interval: u64) -> Result<serde_json::Value> {
    if interval == 0 {
        return Err(anyhow!("worker interval must be greater than zero seconds"));
    }
    WORKER_STOP_REQUESTED.store(false, Ordering::Relaxed);
    let _worker_lease = WorkerLease::acquire(&a.config)?;
    if once {
        let sync = sync_all(a, true)?;
        let results = sync
            .get("results")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        return Ok(serde_json::json!({"worker":"completed","results":results}));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        WORKER_STOP_REQUESTED.store(true, Ordering::Relaxed);
        signal_stop.store(true, Ordering::Relaxed);
    })
    .context("install Ctrl-C handler for worker")?;
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

fn validate_harness_links(state: &State, library: &Path) -> Result<()> {
    for (key, record) in &state.harness_links {
        validate_harness_link_record(key, record, library)?;
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

fn harness_link_health(a: &App) -> Result<Vec<serde_json::Value>> {
    let mut health = Vec::new();
    for (key, record) in &a.state.harness_links {
        let link = PathBuf::from(&record.link_path);
        let expected = PathBuf::from(&record.canonical_path);
        let (status, message) = match fs::symlink_metadata(&link) {
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

fn harness_link(a: &mut App, root: &Path, raw_skill: &str) -> Result<serde_json::Value> {
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

fn harness_unlink(a: &mut App, root: &Path, raw_skill: &str) -> Result<serde_json::Value> {
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

fn requires_lock(command: &Cmd) -> bool {
    match command {
        Cmd::Config {
            command: ConfigCmd::Path,
        }
        | Cmd::Status
        | Cmd::Diff
        | Cmd::Doctor
        | Cmd::Harness {
            command: HarnessCmd::List,
        } => false,
        Cmd::Publish { dry_run, .. } => !dry_run,
        _ => true,
    }
}

fn main() -> Result<()> {
    let json_requested = std::env::args_os()
        .skip(1)
        .any(|argument| argument.to_string_lossy() == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if json_requested {
                let exit_code = error.exit_code();
                envelope(true, false, &error.to_string(), serde_json::json!({}));
                std::process::exit(exit_code);
            }
            error.exit();
        }
    };
    let json = cli.json;
    let result = run(cli);
    match result {
        Ok(v) => envelope(json, true, "ok", v),
        Err(e) => {
            envelope(json, false, &e.to_string(), serde_json::json!({}));
            std::process::exit(1)
        }
    }
    Ok(())
}

fn set_change(
    a: &mut App,
    raw_name: &str,
    raw_skill: &str,
    adding: bool,
) -> Result<serde_json::Value> {
    let name = strict_component(raw_name, "set name")?;
    let skill = strict_component(raw_skill, "skill name")?;
    if !a.state.sets.contains_key(&name) {
        return Err(anyhow!("set not found: {name}"));
    }
    if adding {
        let skill_path = a.library.join(&skill);
        assert_no_symlink_path(&skill_path, Path::new("."))?;
        if !skill_path.is_dir()
            || !checked_regular_path(&skill_path.join("SKILL.md"), "skill manifest")?
        {
            return Err(anyhow!("skill not found in library: {skill}"));
        }
    }
    let id = set_member_id(&skill)?;
    let changed = if adding {
        a.state
            .sets
            .get_mut(&name)
            .expect("set existence checked")
            .members
            .insert(id)
    } else {
        a.state
            .sets
            .get_mut(&name)
            .expect("set existence checked")
            .members
            .remove(&id)
    };
    if changed {
        a.save()?;
    }
    let status = if changed {
        if adding {
            "added"
        } else {
            "removed"
        }
    } else if adding {
        "already_present"
    } else {
        "already_absent"
    };
    Ok(serde_json::json!({"set":name,"skill":skill,"status":status}))
}

fn run(cli: Cli) -> Result<serde_json::Value> {
    let mut a = App::load()?;
    let command = match cli.command {
        Some(command) => command,
        None => {
            return Ok(serde_json::json!({
                "status": if a.state_path.exists() { "ready" } else { "setup_required" },
                "library": a.library,
                "message": if a.state_path.exists() {
                    "Skillsync is ready; use status or open the terminal interface"
                } else {
                    "Skillsync is not initialized; run init"
                }
            }));
        }
    };
    let needs_lock = requires_lock(&command);
    let _state_lock = if needs_lock {
        Some(StateLock::acquire(&a.config)?)
    } else {
        None
    };
    if needs_lock {
        a = App::load()?;
    }
    let result: Result<serde_json::Value> = match command {
        Cmd::Init { library } => {
            if !a.state_path.exists() {
                if let Some(l) = library {
                    assert_no_symlink_path(&l, Path::new("."))?;
                    fs::create_dir_all(&l)?;
                    assert_no_symlink_path(&l, Path::new("."))?;
                    a.library = fs::canonicalize(l)?;
                    a.state.library = a.library.display().to_string()
                }
                assert_no_symlink_path(&a.library, Path::new("."))?;
                fs::create_dir_all(&a.library)?;
                assert_no_symlink_path(&a.library, Path::new("."))?;
                a.library = fs::canonicalize(&a.library)?;
                a.state.library = a.library.display().to_string();
                a.save()?
            }
            let config_path = a.config.join("config.toml");
            if !config_path.exists() {
                atomic(
                    &config_path,
                    format!("library = {:?}\n", a.library.display().to_string()).as_bytes(),
                )?;
            }
            Ok(serde_json::json!({"library":a.library}))
        }
        Cmd::Config {
            command: ConfigCmd::Path,
        } => Ok(serde_json::json!({"path":a.config.join("config.toml")})),
        Cmd::Config {
            command: ConfigCmd::Edit,
        } => {
            let p = a.config.join("config.toml");
            if !p.exists() {
                fs::create_dir_all(&a.config)?;
                atomic(
                    &p,
                    format!("library = {:?}\n", a.library.display().to_string()).as_bytes(),
                )?
            }
            Ok(serde_json::json!({"path":p}))
        }
        Cmd::Subscribe { repository, skill } => {
            let r =
                repository.ok_or_else(|| anyhow!("repository required in noninteractive mode"))?;
            let n = skill.ok_or_else(|| {
                anyhow!("--skill required in noninteractive mode; picker requires a TTY")
            })?;
            let n = skill_query(&n)?;
            let url = normalize(&r)?;
            let (repo, b) = clone_repo(&url)?;
            let (name, src, raw_source_path) = find_skill(repo.path(), &n)?;
            let source_path = source_rel(&raw_source_path)?;
            let key = relationship_key(&url, &source_path);
            strict_component(&name, "manifest skill name")?;
            if let Some(existing) = subscription_overlaps(&a.state, &url, &source_path) {
                return Err(anyhow!(
                    "subscription overlaps existing relationship: {existing}"
                ));
            }
            let dst = a.library.join(&name);
            if !dst.starts_with(&a.library) {
                return Err(anyhow!("destination escaped library"));
            }
            if dst.exists() {
                return Err(anyhow!("local package exists; recovery/decision needed"));
            }
            assert_no_symlink_path(&a.library, Path::new("."))?;
            fs::create_dir_all(&a.library)?;
            assert_no_symlink_path(&a.library, Path::new("."))?;
            let staging_parent = tempfile::tempdir_in(&a.library)?;
            let staged = staging_parent.path().join("package");
            copy_tree(&src, &staged)?;
            let (bp, h) = snapshot(&a, &key, &src)?;
            replace_dir(&dst, &staged)?;
            a.state.subscriptions.insert(
                key.clone(),
                Subscription {
                    skill: name.clone(),
                    source: url,
                    branch: b,
                    source_path,
                    baseline_path: bp.display().to_string(),
                    baseline_hash: h,
                    local_path: dst.display().to_string(),
                    status: "synced".into(),
                    recovery_path: None,
                    last_sync: now(),
                    update_count: 0,
                },
            );
            a.save()?;
            Ok(serde_json::json!({"skill":name}))
        }
        Cmd::Update | Cmd::Sync => sync_all(&mut a, false),
        Cmd::Worker { once, interval } => run_worker_locked(&mut a, once, interval),
        Cmd::Harness { command } => match command {
            HarnessCmd::Link { root, skill } => harness_link(&mut a, &root, &skill),
            HarnessCmd::Unlink { root, skill } => harness_unlink(&mut a, &root, &skill),
            HarnessCmd::List => Ok(serde_json::json!({"links":a.state.harness_links})),
        },
        Cmd::Diff => {
            let mut v = vec![];
            for (key, sub) in &a.state.subscriptions {
                strict_component(&sub.skill, "subscription skill name")?;
                strict_component(key, "subscription relationship key")?;
                let local = PathBuf::from(&sub.local_path);
                let expected_local = a.library.join(&sub.skill);
                if local != expected_local {
                    return Err(anyhow!("subscription local path does not match library"));
                }
                let local_relative = local
                    .strip_prefix(&a.library)
                    .map_err(|_| anyhow!("subscription destination escaped library"))?;
                if !safe(local_relative) {
                    return Err(anyhow!("subscription destination escaped library"));
                }
                assert_no_symlink_path(&a.library, local_relative)?;
                let baseline = PathBuf::from(&sub.baseline_path);
                let expected_baseline = a.baselines.join(key);
                if baseline != expected_baseline {
                    return Err(anyhow!(
                        "subscription baseline path does not match state directory"
                    ));
                }
                validate_state_path(&a.baselines, &baseline, "baseline")?;
                let h = hash_dir(&local)?;
                v.push(serde_json::json!({"skill":key,"status":sub.status,"local_changed":h!=sub.baseline_hash,"baseline":sub.baseline_hash}))
            }
            Ok(serde_json::json!({"results":v}))
        }
        Cmd::Publish {
            skill,
            repo,
            yes,
            dry_run,
        } => {
            if !yes && !dry_run {
                return Err(anyhow!("approval required: pass --yes or --dry-run"));
            }
            let skill = strict_component(&skill, "skill name")?;
            let src = a.library.join(&skill);
            let src_relative = src
                .strip_prefix(&a.library)
                .map_err(|_| anyhow!("publication source escaped library"))?;
            assert_no_symlink_path(&a.library, src_relative)?;
            if !src.is_dir() {
                return Err(anyhow!("skill not found in library"));
            }
            let url = normalize(&repo)?;
            if dry_run {
                let listed = files(&src)?
                    .into_iter()
                    .map(|x| x.0.display().to_string())
                    .collect::<Vec<_>>();
                let branch = branch(&url).unwrap_or_else(|_| "main".into());
                Ok(
                    serde_json::json!({"skill":skill,"source":src,"destination":url,"branch":branch,"path":format!("skills/{skill}"),"files":listed,"warning":"publishes selected package contents; prose and references may contain private material"}),
                )
            } else {
                let previous = a
                    .state
                    .publications
                    .values()
                    .find(|publication| {
                        publication.skill == skill && publication.destination == url
                    })
                    .cloned();
                publish_to_repo(&mut a, &skill, &url, previous.as_ref())
            }
        }
        Cmd::Status => Ok(
            serde_json::json!({"subscriptions":a.state.subscriptions,"publications":a.state.publications,"pending_publications":a.state.pending_publications,"harness_links":a.state.harness_links,"worker":worker_status(&a.config)?}),
        ),
        Cmd::Doctor => Ok(
            serde_json::json!({"git":Command::new("git").arg("--version").output().map(|x|x.status.success()).unwrap_or(false),"config_exists":a.config.exists(),"library_exists":a.library.exists(),"worker":worker_status(&a.config)?,"startup":"unsupported","harness_write_back":"explicit_directory_links_only","harness_discovery":"unsupported","harness_filtering":"unsupported","harness_reload":"unsupported","harness_links":harness_link_health(&a)?,"hermes_autonomous_curation":"unsupported","registries":"unsupported","set_publication":"unsupported","set_subscription_metadata":"unsupported","harness_enablement":"unsupported","personal_library_sync":"unsupported","membership_change_propagation":"unsupported"}),
        ),
        Cmd::Set { command } => match command {
            SetCmd::Create { name } => {
                let name = strict_component(&name, "set name")?;
                if a.state.sets.contains_key(&name) {
                    return Err(anyhow!("set already exists: {name}"));
                }
                a.state.sets.insert(name.clone(), SkillSet::default());
                a.save()?;
                Ok(serde_json::json!({"set":name,"members":[]}))
            }
            SetCmd::List => Ok(serde_json::json!({"sets":a.state.sets.keys().collect::<Vec<_>>()})),
            SetCmd::Show { name } => {
                let name = strict_component(&name, "set name")?;
                let set = a
                    .state
                    .sets
                    .get(&name)
                    .ok_or_else(|| anyhow!("set not found: {name}"))?;
                let members = set
                    .members
                    .iter()
                    .map(|id| set_member_name(id))
                    .collect::<Result<Vec<_>>>()?;
                Ok(serde_json::json!({"set":name,"members":members}))
            }
            SetCmd::Add { name, skill } => set_change(&mut a, &name, &skill, true),
            SetCmd::Remove { name, skill } => set_change(&mut a, &name, &skill, false),
        },
        Cmd::Unsubscribe { skill } => {
            let matches = a
                .state
                .subscriptions
                .iter()
                .filter(|(key, sub)| key.as_str() == skill || sub.skill == skill)
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(anyhow!("subscription not found: {skill}"));
            }
            if matches.len() > 1 {
                return Err(anyhow!(
                    "skill name is ambiguous; use the relationship key: {skill}"
                ));
            }
            a.state.subscriptions.remove(&matches[0]);
            a.save()?;
            Ok(serde_json::json!({"skill":skill,"status":"unsubscribed"}))
        }
        Cmd::Unpublish { skill, repo } => {
            let skill = strict_component(&skill, "skill name")?;
            let url = normalize(&repo)?;
            let mut matches = a
                .state
                .publications
                .iter()
                .filter(|(_, publication)| {
                    publication.skill == skill && publication.destination == url
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            if matches.is_empty() {
                matches = a
                    .state
                    .pending_publications
                    .iter()
                    .filter(|(_, pending)| {
                        pending.publication.skill == skill && pending.publication.destination == url
                    })
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
            }
            if matches.is_empty() {
                return Err(anyhow!("publication not found for {skill} and {url}"));
            }
            if matches.len() > 1 {
                return Err(anyhow!(
                    "publication selection is ambiguous; use a specific relationship key"
                ));
            }
            a.state.publications.remove(&matches[0]);
            a.state.pending_publications.remove(&matches[0]);
            a.save()?;
            Ok(serde_json::json!({"skill":skill,"status":"unpublished; destination retained"}))
        }
    };
    result
}
