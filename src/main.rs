use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod config_editor;
mod filesystem;
mod harness;
mod recovery;

use filesystem::{
    assert_no_symlink_path, atomic, checked_regular_path, copy_existing_tree, copy_tree, discover,
    effective_library_path, files, hash_dir, manifest_name, open_advisory_lock, read_regular_file,
    replace_dir, resolve_library_path, safe, snapshot, source_rel, strict_component,
    sync_managed_tree, validate_state_path, write_file_data, FileData, StateLock, WorkerLease,
};
#[cfg(unix)]
use filesystem::{open_child_file, open_directory_fd};

pub(crate) use recovery::{
    delete_skill, directory_identity, import_local, remove_owned_directory, restore_skill,
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
    Import {
        #[arg(long = "from")]
        source: PathBuf,
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
    Delete {
        skill: String,
        #[arg(long)]
        yes: bool,
    },
    Restore {
        #[arg(long = "from")]
        recovery_path: PathBuf,
        #[arg(long)]
        skill: Option<String>,
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
    Enable {
        #[arg(long)]
        root: PathBuf,
        #[arg(long = "set")]
        set: String,
    },
    Disable {
        #[arg(long)]
        root: PathBuf,
        #[arg(long = "set")]
        set: String,
    },
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
    #[serde(default)]
    harness_sets: BTreeMap<String, HarnessSetEnablement>,
    #[serde(default)]
    local_adoptions: BTreeMap<String, LocalAdoption>,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
struct LocalAdoption {
    skill: String,
    source_path: String,
    source_package: String,
    content_hash: String,
    local_path: String,
    status: String,
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
struct HarnessSetEnablement {
    set: String,
    harness_root: String,
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
struct App {
    config: PathBuf,
    library: PathBuf,
    state_path: PathBuf,
    baselines: PathBuf,
    recovery: PathBuf,
    state: State,
}

static WORKER_STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

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
        let requested_library = effective_library_path(file_cfg.library.map(PathBuf::from));
        let expected_library = resolve_library_path(&requested_library)?;
        let state_exists = checked_regular_path(&sp, "state")?;
        let mut state: State = if state_exists {
            serde_json::from_slice(&read_regular_file(&sp, None)?)?
        } else {
            State {
                version: 5,
                library: expected_library.display().to_string(),
                ..Default::default()
            }
        };
        if state.version > 5 {
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
        if state.version < 5 {
            state.version = 5;
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
        harness::validate_harness_links(&state, &library)?;
        harness::validate_harness_sets(&state, &library)?;
        validate_local_adoptions(&state, &library)?;
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
        if std::env::var("SKILLSYNC_TEST_FAIL_STATE_SAVE").as_deref() == Ok("1") {
            return Err(anyhow!("injected state-save failure (test-only)"));
        }
        atomic(&self.state_path, &serde_json::to_vec_pretty(&self.state)?)
    }
}
fn set_member_id(skill: &str) -> Result<String> {
    Ok(format!(
        "library:{}",
        strict_component(skill, "skill name")?
    ))
}
pub(crate) fn set_member_name(id: &str) -> Result<String> {
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
fn validate_local_adoption(key: &str, record: &LocalAdoption, library: &Path) -> Result<()> {
    let skill = strict_component(&record.skill, "local adoption skill name")?;
    if key != format!("local:{skill}") {
        return Err(anyhow!("local adoption key does not match skill"));
    }
    let source = PathBuf::from(&record.source_path);
    if !source.is_absolute() {
        return Err(anyhow!("local adoption source path is not absolute"));
    }
    let source_metadata = fs::symlink_metadata(&source).ok();
    if source_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(anyhow!("local adoption source path is a symlink"));
    }
    assert_no_symlink_path(&source, Path::new("."))?;
    let source_available = source.is_dir();
    if source_available {
        if fs::canonicalize(&source)? != source {
            return Err(anyhow!("local adoption source path is not canonical"));
        }
    } else if source.exists() {
        return Err(anyhow!("local adoption source path is not a directory"));
    }
    let source_package = source_rel(&record.source_package)?;
    if record.content_hash.len() != 64
        || !record.content_hash.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(anyhow!("invalid local adoption content hash"));
    }
    if record.status != "adopted" {
        return Err(anyhow!("invalid local adoption status"));
    }
    let local = PathBuf::from(&record.local_path);
    let expected = library.join(&skill);
    if local != expected || !local.is_absolute() {
        return Err(anyhow!("local adoption path does not match library"));
    }
    let relative = local
        .strip_prefix(library)
        .map_err(|_| anyhow!("local adoption escaped library"))?;
    assert_no_symlink_path(library, relative)?;
    if !local.is_dir() || fs::canonicalize(&local)? != local {
        return Err(anyhow!(
            "local adoption canonical path is not a regular directory"
        ));
    }
    if manifest_name(&local)? != skill || hash_dir(&local)? != record.content_hash {
        return Err(anyhow!(
            "local adoption content does not match recorded provenance"
        ));
    }
    if source_available {
        let package = source.join(&source_package);
        if !package.is_dir()
            || fs::canonicalize(&package)? != package
            || manifest_name(&package)? != skill
            || hash_dir(&package)? != record.content_hash
        {
            return Err(anyhow!(
                "local adoption source package does not match recorded provenance"
            ));
        }
    }
    Ok(())
}
fn validate_local_adoptions(state: &State, library: &Path) -> Result<()> {
    for (key, record) in &state.local_adoptions {
        validate_local_adoption(key, record, library)?;
    }
    Ok(())
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
fn interactive_package_selection(found: &[(String, PathBuf, String)]) -> Result<usize> {
    if found.is_empty() {
        return Err(anyhow!("no skill packages found in repository"));
    }
    println!("Found {} skill package(s):", found.len());
    for (index, (name, _, rel)) in found.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, name, rel);
    }
    println!("Select one package by number (full TUI: unsupported; empty or q cancels):");
    let mut input = String::new();
    std::io::stdin().lock().read_line(&mut input)?;
    let value = input.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("q") || value.eq_ignore_ascii_case("cancel") {
        return Err(anyhow!("subscription cancelled"));
    }
    let number = value
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid package selection: {value}"))?;
    if number == 0 || number > found.len() {
        return Err(anyhow!("invalid package selection: {value}"));
    }
    Ok(number - 1)
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
fn select_discovered<'a>(
    found: &'a [(String, PathBuf, String)],
    raw_query: &str,
) -> Result<&'a (String, PathBuf, String)> {
    let query = skill_query(raw_query)?;
    let path_matches = found
        .iter()
        .filter(|(_, _, rel)| rel == &query)
        .collect::<Vec<_>>();
    let matches = if !path_matches.is_empty() {
        path_matches
    } else {
        found
            .iter()
            .filter(|(name, _, _)| name == &query)
            .collect::<Vec<_>>()
    };
    match matches.as_slice() {
        [] => Err(anyhow!("skill not found in repository: {query}")),
        [one] => Ok(one),
        many => Err(anyhow!(
            "skill selection is ambiguous: {}; candidates: {}",
            query,
            many.iter()
                .map(|(_, _, rel)| rel.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
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
        Cmd::Restore { .. } => true,
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
    if let Cmd::Subscribe { repository, skill } = &command {
        use std::io::IsTerminal;
        let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if (!interactive || cli.json) && (repository.is_none() || skill.is_none()) {
            return Err(anyhow!("repository and --skill are required in noninteractive mode; picker requires interactive stdin and stdout"));
        }
    }
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
        } => config_editor::edit_config(&a, cli.json),
        Cmd::Subscribe { repository, skill } => {
            use std::io::IsTerminal;
            let interactive =
                std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && !cli.json;
            let r = match repository {
                Some(repository) => repository,
                None if interactive => {
                    println!("Repository:");
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let input = input.trim();
                    if input.is_empty() {
                        return Err(anyhow!("subscription cancelled"));
                    }
                    input.to_owned()
                }
                None => return Err(anyhow!("repository required in noninteractive mode")),
            };
            let url = normalize(&r)?;
            let (repo, b) = clone_repo(&url)?;
            let found = discover(repo.path())?;
            let selected = match skill {
                Some(n) => select_discovered(&found, &n)?,
                None if interactive => &found[interactive_package_selection(&found)?],
                None => {
                    return Err(anyhow!(
                        "--skill required in noninteractive mode; picker requires a TTY"
                    ))
                }
            };
            let (name, src, raw_source_path) = selected;
            let source_path = source_rel(raw_source_path)?;
            let key = relationship_key(&url, &source_path);
            strict_component(name, "manifest skill name")?;
            if let Some(existing) = subscription_overlaps(&a.state, &url, &source_path) {
                return Err(anyhow!(
                    "subscription overlaps existing relationship: {existing}"
                ));
            }
            let dst = a.library.join(name);
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
            copy_tree(src, &staged)?;
            let (bp, h) = snapshot(&a, &key, src)?;
            let baseline_identity = directory_identity(&bp)?;
            replace_dir(&dst, &staged)?;
            let installed_identity = directory_identity(&dst)?;
            let installed_hash = hash_dir(&dst)?;
            let previous_state = a.state.clone();
            a.state.subscriptions.insert(
                key.clone(),
                Subscription {
                    skill: name.clone(),
                    source: url,
                    branch: b,
                    source_path,
                    baseline_path: bp.display().to_string(),
                    baseline_hash: h.clone(),
                    local_path: dst.display().to_string(),
                    status: "synced".into(),
                    recovery_path: None,
                    last_sync: now(),
                    update_count: 0,
                },
            );
            if let Err(error) = a.save() {
                a.state = previous_state;
                let mut cleanup_errors = Vec::new();
                if let Err(cleanup_error) = remove_owned_directory(
                    &a.library,
                    &dst,
                    Some(installed_identity),
                    &installed_hash,
                ) {
                    cleanup_errors.push(cleanup_error.to_string());
                }
                if let Err(cleanup_error) =
                    remove_owned_directory(&a.baselines, &bp, Some(baseline_identity), &h)
                {
                    cleanup_errors.push(cleanup_error.to_string());
                }
                return if cleanup_errors.is_empty() {
                    Err(error)
                        .context("persist subscription state; package and baseline rolled back")
                } else {
                    Err(anyhow!(
                        "persist subscription state failed: {error}; {}",
                        cleanup_errors.join("; ")
                    ))
                };
            }
            Ok(serde_json::json!({"skill":name}))
        }
        Cmd::Import { source, skill } => import_local(&mut a, &source, skill.as_deref()),
        Cmd::Delete { skill, yes } => delete_skill(&mut a, &skill, yes),
        Cmd::Restore {
            recovery_path,
            skill,
        } => restore_skill(&a, &recovery_path, skill.as_deref()),
        Cmd::Update | Cmd::Sync => sync_all(&mut a, false),
        Cmd::Worker { once, interval } => run_worker_locked(&mut a, once, interval),
        Cmd::Harness { command } => match command {
            HarnessCmd::Enable { root, set } => harness::harness_set(&mut a, &root, &set, true),
            HarnessCmd::Disable { root, set } => harness::harness_set(&mut a, &root, &set, false),
            HarnessCmd::Link { root, skill } => harness::harness_link(&mut a, &root, &skill),
            HarnessCmd::Unlink { root, skill } => harness::harness_unlink(&mut a, &root, &skill),
            HarnessCmd::List => harness::list(&a),
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
            serde_json::json!({"subscriptions":a.state.subscriptions,"local_adoptions":a.state.local_adoptions,"publications":a.state.publications,"pending_publications":a.state.pending_publications,"harness_links":a.state.harness_links,"harness_sets":a.state.harness_sets,"harness_health":harness::harness_link_health(&a)?,"worker":worker_status(&a.config)?}),
        ),
        Cmd::Doctor => Ok(
            serde_json::json!({"git":Command::new("git").arg("--version").output().map(|x|x.status.success()).unwrap_or(false),"config_exists":a.config.exists(),"library_exists":a.library.exists(),"worker":worker_status(&a.config)?,"startup":"unsupported","subscribe_picker":"supported: TTY line-oriented single-select; unattended onboarding unsupported; full TUI: unsupported","harness_write_back":"explicit_directory_links_only","harness_discovery":"unsupported","harness_filtering":"unsupported","harness_reload":"unsupported","harness_links":harness::harness_link_health(&a)?,"hermes_autonomous_curation":"unsupported","registries":"unsupported","set_publication":"unsupported","set_subscription_metadata":"unsupported","harness_enablement":"supported: explicit one-time set expansion into native directory links","personal_library_sync":"unsupported","membership_change_propagation":"unsupported","local_import":"supported: explicit --from PATH --skill NAME; canonical write-back only; no subscription"}),
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
