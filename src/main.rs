use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::BufRead,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod config_editor;
mod conflicts;
mod filesystem;
mod harness;
mod inventory;
mod recovery;
mod repository;
mod tui;

use filesystem::{
    assert_no_symlink_path, atomic, canonicalize_path, checked_regular_path, copy_tree, discover,
    effective_library_path, files, hash_dir, manifest_name, open_advisory_lock, read_regular_file,
    replace_dir_bound, resolve_library_path, safe, snapshot_transaction, source_rel,
    strict_component, validate_state_path, FileData, StateLock, WorkerLease,
};
#[cfg(unix)]
use filesystem::{open_child_file, open_directory_fd};

pub(crate) use recovery::{delete_skill, import_local, restore_skill};

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
    Inventory,
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
    Conflicts {
        #[command(subcommand)]
        command: ConflictCmd,
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
enum ConflictCmd {
    List,
    Show { relationship: String },
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
    #[serde(default)]
    baseline_source: String,
    #[serde(default)]
    baseline_source_path: String,
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
    pub(crate) config: PathBuf,
    pub(crate) library: PathBuf,
    pub(crate) state_path: PathBuf,
    baselines: PathBuf,
    recovery: PathBuf,
    state: State,
    #[cfg(unix)]
    config_directory: fs::File,
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
pub(crate) fn config_dir() -> PathBuf {
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
    pub(crate) fn load(anchor: Option<&StateLock>) -> Result<Self> {
        let c = config_dir();
        if let Some(anchor) = anchor {
            // The lock is anchored to the directory inode, not merely its
            // pathname. Refuse to consume config/state through a replacement
            // pathname before and after every load phase.
            anchor.verify_config_identity(&c)?;
        }
        #[cfg(unix)]
        if anchor.is_none() {
            assert_no_symlink_path(&c, Path::new("."))?;
        }
        #[cfg(not(unix))]
        assert_no_symlink_path(&c, Path::new("."))?;
        let sp = c.join("state.json");
        let cfg_path = c.join("config.toml");
        #[cfg(unix)]
        let config_directory = match anchor {
            Some(lock) => lock.directory_try_clone()?,
            None => filesystem::open_directory_file(&c)?,
        };
        let config_exists = checked_regular_path(&cfg_path, "config")?;
        let file_cfg = if config_exists {
            let contents = String::from_utf8({
                #[cfg(unix)]
                {
                    filesystem::read_relative_file(&config_directory, "config.toml")?
                }
                #[cfg(not(unix))]
                {
                    read_regular_file(&cfg_path, None)?
                }
            })
            .context("config.toml is not valid UTF-8")?;
            toml::from_str::<FileConfig>(&contents).context("invalid config.toml")?
        } else {
            FileConfig::default()
        };
        let requested_library = effective_library_path(file_cfg.library.map(PathBuf::from));
        let expected_library = resolve_library_path(&requested_library)?;
        let state_exists = checked_regular_path(&sp, "state")?;
        let mut state: State = if state_exists {
            serde_json::from_slice(&{
                #[cfg(unix)]
                {
                    filesystem::read_relative_file(&config_directory, "state.json")?
                }
                #[cfg(not(unix))]
                {
                    read_regular_file(&sp, None)?
                }
            })?
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
            #[cfg(unix)]
            config_directory,
        })
    }
    fn save(&self) -> Result<()> {
        #[cfg(feature = "test-hooks")]
        if std::env::var("SKILLSYNC_TEST_FAIL_STATE_SAVE").as_deref() == Ok("1") {
            return Err(anyhow!("injected state-save failure (test-only)"));
        }
        #[cfg(unix)]
        {
            filesystem::write_relative_file_fd(
                &self.config_directory,
                "state.json",
                &serde_json::to_vec_pretty(&self.state)?,
            )
        }
        #[cfg(not(unix))]
        {
            atomic(&self.state_path, &serde_json::to_vec_pretty(&self.state)?)
        }
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
        if canonicalize_path(&source)? != source {
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
    if !local.is_dir() || canonicalize_path(&local)? != local {
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
            || canonicalize_path(&package)? != package
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
pub(crate) fn relationship_key(source: &str, source_path: &str) -> String {
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
        match repository::update_one(a, &key) {
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
        match repository::publish_to_repo(a, &intent.skill, &intent.destination, previous.as_ref())
        {
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
        match repository::publish_to_repo(a, &skill, &destination, Some(&previous)) {
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
        | Cmd::Inventory
        | Cmd::Diff
        | Cmd::Doctor
        | Cmd::Harness {
            command: HarnessCmd::List,
        } => false,
        Cmd::Publish { dry_run, .. } => !dry_run,
        Cmd::Restore { .. } => true,
        Cmd::Conflicts {
            command: ConflictCmd::List | ConflictCmd::Show { .. },
        } => true,
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
    if cli.command.is_none() && !json {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            return tui::run();
        }
    }
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
    let command = match cli.command {
        Some(command) => command,
        None => {
            let a = App::load(None)?;
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
    let conflicts_read_lock = matches!(
        command,
        Cmd::Conflicts {
            command: ConflictCmd::List | ConflictCmd::Show { .. }
        }
    );
    let inventory_read = matches!(command, Cmd::Inventory);
    let needs_lock = requires_lock(&command) && !conflicts_read_lock && !inventory_read;
    let _state_lock = if needs_lock {
        Some(StateLock::acquire(&config_dir())?)
    } else {
        None
    };
    let conflicts_read_lock = if conflicts_read_lock {
        StateLock::acquire_read_only_if_present(&config_dir())?
    } else {
        None
    };
    let inventory_read_lock = if inventory_read {
        StateLock::acquire_read_only_if_present(&config_dir())?
    } else {
        None
    };
    if let Some(lock) = &conflicts_read_lock {
        lock.verify_config_identity(&config_dir())?;
    }
    if let Some(lock) = &inventory_read_lock {
        lock.verify_config_identity(&config_dir())?;
    }
    let operation_lock = _state_lock
        .as_ref()
        .or(conflicts_read_lock.as_ref())
        .or(inventory_read_lock.as_ref());
    if operation_lock.is_none()
        && (matches!(
            command,
            Cmd::Conflicts {
                command: ConflictCmd::List | ConflictCmd::Show { .. }
            }
        ) || matches!(command, Cmd::Inventory))
    {
        let config_path = config_dir();
        let state_path = config_path.join("state.json");
        let config_exists = config_path.is_dir() && config_path.join("config.toml").is_file();
        let state_exists = config_path.is_dir() && state_path.is_file();
        if state_exists || config_exists {
            return Err(anyhow!(
                "cannot inventory initialized Skillsync state without the shared lock"
            ));
        }
        if matches!(command, Cmd::Inventory) {
            return Ok(serde_json::to_value(inventory::query_uninitialized()?)?);
        }
        if matches!(
            command,
            Cmd::Conflicts {
                command: ConflictCmd::List | ConflictCmd::Show { .. }
            }
        ) {
            return Ok(serde_json::json!({"conflicts": [], "count": 0}));
        }
    }
    let mut a = App::load(operation_lock)?;
    if let Some(lock) = &conflicts_read_lock {
        lock.verify_config_identity(&a.config)?;
    }
    if let Some(lock) = &inventory_read_lock {
        lock.verify_config_identity(&a.config)?;
    }
    let result: Result<serde_json::Value> = match command {
        Cmd::Init { library } => {
            if !a.state_path.exists() {
                if let Some(l) = library {
                    assert_no_symlink_path(&l, Path::new("."))?;
                    fs::create_dir_all(&l)?;
                    assert_no_symlink_path(&l, Path::new("."))?;
                    a.library = canonicalize_path(&l)?;
                    a.state.library = a.library.display().to_string()
                }
                assert_no_symlink_path(&a.library, Path::new("."))?;
                fs::create_dir_all(&a.library)?;
                assert_no_symlink_path(&a.library, Path::new("."))?;
                a.library = canonicalize_path(&a.library)?;
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
            let url = repository::normalize(&r)?;
            let (repo, b) = repository::clone_repo(&url)?;
            let found = discover(repo.path())?;
            let selected = match skill {
                Some(n) => repository::select_discovered(&found, &n)?,
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
            let library_parent = filesystem::open_directory_file_bound(&a.library)?;
            let staging_parent = tempfile::tempdir_in(&a.library)?;
            let staged = staging_parent.path().join("package");
            copy_tree(src, &staged)?;
            let (bp, h, mut baseline_replacement) = snapshot_transaction(&a, &key, src)?;
            let mut live_replacement = replace_dir_bound(&dst, &staged, &library_parent)?;
            let previous_state = a.state.clone();
            a.state.subscriptions.insert(
                key.clone(),
                Subscription {
                    skill: name.clone(),
                    source: url.clone(),
                    branch: b,
                    source_path: source_path.clone(),
                    baseline_path: bp.display().to_string(),
                    baseline_hash: h.clone(),
                    baseline_source: url,
                    baseline_source_path: source_path,
                    local_path: dst.display().to_string(),
                    status: "synced".into(),
                    recovery_path: None,
                    last_sync: now(),
                    update_count: 0,
                },
            );
            if let Err(error) = a.save() {
                a.state = previous_state;
                drop(live_replacement);
                drop(baseline_replacement);
                return Err(error)
                    .context("persist subscription state; package and baseline rolled back");
            }
            live_replacement.commit()?;
            baseline_replacement.commit()?;
            Ok(serde_json::json!({"skill":name}))
        }
        Cmd::Import { source, skill } => import_local(&mut a, &source, skill.as_deref()),
        Cmd::Delete { skill, yes } => delete_skill(&mut a, &skill, yes),
        Cmd::Restore {
            recovery_path,
            skill,
        } => restore_skill(&a, &recovery_path, skill.as_deref()),
        Cmd::Conflicts {
            command: ConflictCmd::Show { relationship },
        } => conflicts::show(&a, &relationship),
        Cmd::Conflicts {
            command: ConflictCmd::List,
        } => conflicts::list(&a),
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
            let url = repository::normalize(&repo)?;
            if dry_run {
                let listed = files(&src)?
                    .into_iter()
                    .map(|x| x.0.display().to_string())
                    .collect::<Vec<_>>();
                let branch = repository::branch(&url).unwrap_or_else(|_| "main".into());
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
                repository::publish_to_repo(&mut a, &skill, &url, previous.as_ref())
            }
        }
        Cmd::Status => Ok(
            serde_json::json!({"subscriptions":a.state.subscriptions,"local_adoptions":a.state.local_adoptions,"publications":a.state.publications,"pending_publications":a.state.pending_publications,"harness_links":a.state.harness_links,"harness_sets":a.state.harness_sets,"harness_health":harness::harness_link_health(&a)?,"worker":worker_status(&a.config)?}),
        ),
        Cmd::Inventory => Ok(serde_json::to_value(inventory::query(&a)?)?),
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
            let url = repository::normalize(&repo)?;
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
