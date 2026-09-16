use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
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
    if let Some(x) = p.parent() {
        fs::create_dir_all(x)?
    }
    let t = p.with_file_name(format!(
        ".{}.tmp-{}",
        p.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    let mut f = fs::File::create(&t)?;
    f.write_all(b)?;
    f.sync_all()?;
    fs::rename(t, p)?;
    Ok(())
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
                version: 3,
                library: expected_library.display().to_string(),
                ..Default::default()
            }
        };
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
        || s.contains('/')
        || s.contains('\\')
        || s.contains(':')
        || Path::new(s).is_absolute()
    {
        return Err(anyhow!("invalid {what}: {s:?}"));
    }
    Ok(s.to_owned())
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
    fn r(root: &Path, p: &Path, o: &mut Vec<(PathBuf, Vec<u8>, u32)>) -> Result<()> {
        for e in fs::read_dir(p)? {
            let e = e?;
            let x = e.path();
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
fn checked_regular_path(path: &Path, label: &str) -> Result<bool> {
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(d, fs::Permissions::from_mode(m))?;
        }
    }
    Ok(())
}
fn copy_existing_tree(src: &Path, dst: &Path) -> Result<()> {
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
    fn r(base: &Path, p: &Path, o: &mut Vec<(String, PathBuf, String)>) -> Result<()> {
        for e in fs::read_dir(p)? {
            let x = e?.path();
            let rel = x.strip_prefix(base)?;
            if operational(rel) {
                continue;
            }
            let metadata = fs::symlink_metadata(&x)?;
            if metadata.file_type().is_symlink() {
                return Err(anyhow!("symlink package path rejected: {}", rel.display()));
            }
            if metadata.is_dir() {
                if x.join("SKILL.md").is_file() {
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
    if root.join("SKILL.md").is_file() {
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
fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut c = Command::new("git");
    if let Some(x) = cwd {
        c.current_dir(x);
    }
    let o = c.args(args).output().context("git is not installed")?;
    if !o.status.success() {
        let e = String::from_utf8_lossy(&o.stderr).replace('\n', " ");
        return Err(anyhow!("git operation failed: {}", e.trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().into())
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
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, io::RawFd},
    };
    let parent = dst
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent"))?;
    let name = dst
        .file_name()
        .ok_or_else(|| anyhow!("destination has no name"))?;
    let parent_c = CString::new(parent.as_os_str().as_bytes())?;
    let name_c = CString::new(name.as_bytes())?;
    let fd: RawFd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = (|| {
        let source_c = CString::new(src.as_os_str().as_bytes())?;
        let status =
            unsafe { libc::renameat(libc::AT_FDCWD, source_c.as_ptr(), fd, name_c.as_ptr()) };
        if status < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    })();
    unsafe {
        libc::close(fd);
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
fn merge_tree(base: &Path, local: &Path, up: &Path, out: &Path) -> Result<bool> {
    copy_tree(local, out)?;
    let mut paths = BTreeMap::<PathBuf, (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>)>::new();
    for root in [base, local, up] {
        for (r, b, _) in files(root)? {
            let e = paths.entry(r).or_insert((None, None, None));
            if root == base {
                e.0 = Some(b)
            } else if root == local {
                e.1 = Some(b)
            } else {
                e.2 = Some(b)
            }
        }
    }
    let mut conflict = false;
    for (r, (b, l, u)) in paths {
        if l == u {
            continue;
        }
        if l == b {
            if let Some(x) = u {
                let d = out.join(&r);
                if let Some(parent) = d.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(d, x)?
            } else {
                let _ = fs::remove_file(out.join(&r));
            }
        } else if u == b {
            continue;
        } else if b.is_some() && l.is_some() && u.is_some() {
            let td = tempfile::tempdir()?;
            let x = td.path().join("b");
            let y = td.path().join("l");
            let z = td.path().join("u");
            fs::write(&x, b.unwrap())?;
            fs::write(&y, l.unwrap())?;
            fs::write(&z, u.unwrap())?;
            let o = Command::new("git")
                .args([
                    "merge-file",
                    "-p",
                    y.to_str().unwrap(),
                    x.to_str().unwrap(),
                    z.to_str().unwrap(),
                ])
                .output()?;
            if o.status.code() == Some(0) {
                let d = out.join(&r);
                if let Some(parent) = d.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(d, o.stdout)?
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
    let source_files = files(&source)?;
    let current_hash = hash_dir(&source)?;
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
    let removed = sync_managed_tree(&source, &staged_destination)?;
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
    let changed = cached_changes(tmp.path())?;
    if changed {
        run_git(
            Some(tmp.path()),
            &["commit", "-m", &format!("Update skill {skill}")],
        )?;
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
    let key = publication_key(
        &publication.skill,
        &publication.destination,
        &publication.branch,
        &publication.path,
    );
    a.state.publications.insert(key, publication);
    a.save()?;
    Ok(serde_json::json!({"skill":skill,"status":"published"}))
}
fn main() -> Result<()> {
    let cli = Cli::parse();
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
                fs::write(
                    &p,
                    format!("library = {:?}\n", a.library.display().to_string()),
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
        Cmd::Update | Cmd::Sync => {
            let keys = a.state.subscriptions.keys().cloned().collect::<Vec<_>>();
            let mut v = vec![];
            for k in keys {
                v.push(update_one(&mut a, &k)?)
            }
            let publications = a
                .state
                .publications
                .iter()
                .filter(|(_, publication)| publication.approved)
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
                match publish_to_repo(&mut a, &skill, &destination, Some(&previous)) {
                    Ok(result) => v.push(result),
                    Err(error) => {
                        let status = if error.to_string().to_lowercase().contains("auth") {
                            "authentication_required"
                        } else if error.to_string().to_lowercase().contains("offline") {
                            "offline"
                        } else {
                            "conflict"
                        };
                        if let Some(publication) = a.state.publications.get_mut(&key) {
                            publication.status = status.into();
                            publication.last_sync = now();
                        }
                        a.save()?;
                        v.push(
                            serde_json::json!({"skill":skill,"status":status,"error":error.to_string()}),
                        );
                    }
                }
            }
            Ok(serde_json::json!({"results":v}))
        }
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
            serde_json::json!({"subscriptions":a.state.subscriptions,"publications":a.state.publications,"worker":"stopped/manual-only"}),
        ),
        Cmd::Doctor => Ok(
            serde_json::json!({"git":Command::new("git").arg("--version").output().map(|x|x.status.success()).unwrap_or(false),"config_exists":a.config.exists(),"library_exists":a.library.exists(),"worker":"unsupported","harness_write_back":"unsupported","hermes_autonomous_curation":"unsupported","registries":"unsupported"}),
        ),
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
            let matches = a
                .state
                .publications
                .iter()
                .filter(|(_, publication)| {
                    publication.skill == skill && publication.destination == url
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(anyhow!("publication not found for {skill} and {url}"));
            }
            if matches.len() > 1 {
                return Err(anyhow!(
                    "publication selection is ambiguous; use a specific relationship key"
                ));
            }
            a.state.publications.remove(&matches[0]);
            a.save()?;
            Ok(serde_json::json!({"skill":skill,"status":"unpublished; destination retained"}))
        }
    };
    result
}
