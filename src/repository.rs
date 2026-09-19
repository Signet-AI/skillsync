use crate::filesystem::{
    assert_no_symlink_path, canonicalize_path, copy_existing_tree, copy_tree, discover, files,
    hash_dir, replace_dir_bound, safe, snapshot_transaction, source_rel, strict_component,
    sync_managed_tree, validate_state_path, write_file_data,
};
use crate::publication_key;
use crate::recovery::{directory_identity, remove_owned_directory};
use crate::{
    branch_policy::BranchPolicy, now, unique_stamp, App, FileData, PendingPublication, Publication,
    WORKER_STOP_REQUESTED,
};
use anyhow::{anyhow, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::Duration,
};

pub(crate) fn validate_portable_source_identity(s: &str) -> Result<()> {
    if s.is_empty()
        || s.chars().any(|c| c.is_control())
        || s.contains('\\')
        || s.starts_with('/')
        || s.starts_with("\\\\")
        || (s.len() >= 2 && s.as_bytes()[1] == b':')
        || Path::new(s).is_absolute()
    {
        return Err(anyhow!("nonportable repository source"));
    }
    if s.starts_with("git@") && s.contains(':') {
        return Ok(());
    }
    let Some((scheme, rest)) = s.split_once("://") else {
        return Err(anyhow!("nonportable repository source"));
    };
    if !matches!(scheme, "http" | "https" | "ssh") || rest.is_empty() || rest.contains('@') {
        return Err(anyhow!("nonportable repository source"));
    }
    if rest
        .split('/')
        .next()
        .is_some_and(|authority| authority.is_empty())
    {
        return Err(anyhow!("nonportable repository source"));
    }
    Ok(())
}

pub(crate) fn normalize(s: &str) -> Result<String> {
    if s.is_empty() || s.chars().any(|c| c.is_control()) {
        return Err(anyhow!("unsupported repository source"));
    }
    if Path::new(s).exists() {
        let path = canonicalize_path(Path::new(s))?;
        #[cfg(windows)]
        {
            return Ok(path.to_string_lossy().replace('\\', "/"));
        }
        #[cfg(not(windows))]
        return Ok(path.display().to_string());
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
pub(crate) fn validate_branch(branch: &str) -> Result<()> {
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
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(anyhow!("git operation failed: {detail}"));
    }
    Ok(String::from_utf8_lossy(&stdout).trim().into())
}
pub(crate) fn status_for_error(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("auth") || lower.contains("could not read username") {
        "authentication_required"
    } else if lower.contains("offline")
        || lower.contains("not installed")
        || lower.contains("could not resolve host")
        || lower.contains("network is unreachable")
        || lower.contains("connection timed out")
    {
        "offline"
    } else if lower.contains("permission")
        || lower.contains("access denied")
        || lower.contains("could not create work tree dir")
    {
        "permission_denied"
    } else if error.contains("source repository missing") {
        "source_missing"
    } else if error.contains("tracked branch missing") {
        "branch_missing"
    } else if error.contains("skill not found") {
        "package_missing"
    } else if lower.contains("unsupported repository")
        || lower.contains("repository url contains")
        || lower.contains("credential-bearing repository url rejected")
        || lower.contains("unsafe git branch")
        || lower.contains("invalid repository source")
    {
        "invalid_source"
    } else {
        "conflict"
    }
}
fn recovered_from(status: &str) -> Option<&'static str> {
    match status {
        "source_missing" => Some("source_missing"),
        "branch_missing" => Some("branch_missing"),
        "package_missing" => Some("package_missing"),
        _ => None,
    }
}

pub(crate) fn branch(repo: &str) -> Result<String> {
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
pub(crate) fn clone_repo(repo: &str) -> Result<(tempfile::TempDir, String)> {
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
pub(crate) fn clone_repo_branch_for_subscribe(
    repo: &str,
    policy: Option<&BranchPolicy>,
) -> Result<(tempfile::TempDir, String)> {
    let requested = policy.and_then(BranchPolicy::requested_branch);
    if !repo.contains("://") && !repo.starts_with("git@") && !Path::new(repo).exists() {
        return Err(anyhow!("source repository missing"));
    }
    let (t, default) = clone_repo(repo)?;
    if let Some(b) = requested {
        validate_branch(b)?;
        if run_git(
            Some(t.path()),
            &["show-ref", "--verify", &format!("refs/remotes/origin/{b}")],
        )
        .is_err()
        {
            return Err(anyhow!("tracked branch missing"));
        }
        run_git(
            Some(t.path()),
            &["checkout", "--quiet", "-B", b, &format!("origin/{b}")],
        )?;
        return Ok((t, b.to_owned()));
    }
    Ok((t, default))
}
fn clone_repo_branch(repo: &str, requested: Option<&str>) -> Result<(tempfile::TempDir, String)> {
    clone_repo_branch_for_subscribe(
        repo,
        requested.map(BranchPolicy::explicit).transpose()?.as_ref(),
    )
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
pub(crate) fn rev_parse_provenance(repo: &Path, source_path: &str) -> Result<(String, String)> {
    let validate = |value: String, label: &str| {
        if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            Err(anyhow!("invalid Git {label} object id"))
        } else {
            Ok(value)
        }
    };
    let commit = validate(run_git(Some(repo), &["rev-parse", "HEAD"])?, "commit")?;
    let spec = if source_path == "." {
        "HEAD^{tree}".to_owned()
    } else {
        format!("HEAD:{source_path}")
    };
    let tree = validate(run_git(Some(repo), &["rev-parse", &spec])?, "tree")?;
    Ok((commit, tree))
}

pub(crate) fn skill_query(s: &str) -> Result<String> {
    if s.contains('/') || s.contains('\\') {
        let p = s.replace('\\', "/");
        source_rel(&p)
    } else {
        strict_component(s, "skill name")
    }
}
pub(crate) fn select_discovered<'a>(
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
pub(crate) fn update_one(a: &mut App, key: &str) -> Result<serde_json::Value> {
    let mut s = a
        .state
        .subscriptions
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow!("subscription not found"))?;
    let previous_status = s.status.clone();
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
    let record_failure =
        |a: &mut App, s: &mut crate::Subscription, status: &str| -> Result<serde_json::Value> {
            s.status = status.into();
            let output = serde_json::json!({"skill":s.skill,"relationship":key,"status":status});
            a.state.subscriptions.insert(key.into(), s.clone());
            a.save()?;
            Ok(output)
        };
    let stored_policy = s
        .branch_policy
        .as_ref()
        .ok_or_else(|| anyhow!("subscription branch policy missing"))?;
    stored_policy.validate_effective_branch(&s.branch)?;
    let policy = stored_policy.clone();
    let (repo, b) = match clone_repo_branch_for_subscribe(&s.source, Some(&policy)) {
        Ok(x) => x,
        Err(e) if WORKER_STOP_REQUESTED.load(Ordering::Relaxed) => return Err(e),
        Err(e) => {
            let status = if s.source.starts_with("https://")
                && s.source
                    .split_once("://")
                    .and_then(|(_, rest)| rest.split_once('@'))
                    .is_some_and(|(user, _)| user.contains(':'))
            {
                "authentication_required"
            } else if s.source.to_lowercase().contains("permission") {
                "permission_denied"
            } else {
                status_for_error(&e.to_string())
            };
            return record_failure(a, &mut s, status);
        }
    };
    let previous_branch = s.branch.clone();
    let repo_path = crate::filesystem::canonicalize_path_with_missing(repo.path())
        .context("canonicalize update repository")?;
    let (_, up, _) = match find_skill(&repo_path, &s.source_path) {
        Ok(found) => found,
        Err(error) if error.to_string().starts_with("skill not found") => {
            return record_failure(a, &mut s, "package_missing");
        }
        Err(error) => return Err(error),
    };
    let (resolved_commit, resolved_tree) = rev_parse_provenance(&repo_path, &s.source_path)?;
    s.branch = b.clone();
    s.resolved_commit = Some(resolved_commit);
    s.resolved_tree = Some(resolved_tree);
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
        let result = if b != previous_branch {
            "no_change"
        } else {
            s.status.as_str()
        };
        let mut output = serde_json::json!({"skill":s.skill,"relationship":key,"status":s.status,"result":result});
        if let Some(status) = recovered_from(&previous_status) {
            output["recovered_from"] = serde_json::Value::String(status.into());
        }
        return Ok(output);
    }
    let live_parent = crate::filesystem::open_directory_file_bound(
        local
            .parent()
            .ok_or_else(|| anyhow!("subscription path has no parent"))?,
    )?;
    let stage_parent = local
        .parent()
        .ok_or_else(|| anyhow!("subscription path has no parent"))?;
    fs::create_dir_all(stage_parent)?;
    let stage = tempfile::tempdir_in(stage_parent)?;
    let conflict = merge_tree(&base, &local, &up, stage.path())?;
    let mut replacement = None;
    let mut baseline_replacement = None;
    let previous_state = a.state.clone();
    let mut conflict_artifact: Option<(PathBuf, crate::recovery::DirectoryIdentity, String)> = None;
    if conflict {
        if s.status == "conflict" && s.recovery_path.is_some() {
            // An open conflict is immutable evidence. Validate it before reusing it;
            // never replace a damaged artifact with a new snapshot.
            let existing = crate::conflicts::show(a, key)?;
            if existing
                .get("live_hash_at_detection")
                .and_then(|v| v.as_str())
                == Some(lh.as_str())
            {
                return Ok(
                    serde_json::json!({"skill":s.skill,"relationship":key,"status":"conflict"}),
                );
            }
            return Err(anyhow!("existing conflict evidence is invalid or stale"));
        }
        assert_no_symlink_path(&a.recovery, Path::new("."))?;
        fs::create_dir_all(&a.recovery)?;
        assert_no_symlink_path(&a.recovery, Path::new("."))?;
        let rec = a.recovery.join(format!("{}-{}", key, unique_stamp()));
        validate_state_path(&a.recovery, &rec, "recovery")?;
        fs::create_dir_all(&rec)?;
        // Ownership is established before any untrusted evidence copy. If a
        // later step fails, retain this directory unless identity-safe cleanup
        // can prove it is still ours.
        let rec_identity = directory_identity(&rec)?;
        copy_tree(&base, &rec.join("base")).context("copy base evidence; recovery required")?;
        copy_tree(&local, &rec.join("local")).context("copy local evidence; recovery required")?;
        copy_tree(&up, &rec.join("incoming"))
            .context("copy incoming evidence; recovery required")?;
        let manifest = serde_json::json!({"manifest_version":1,"relationship":key,"source":s.source,"source_path":s.source_path,"path":s.local_path,"base_hash":s.baseline_hash,"base_path":rec.join("base"),"local_hash":lh,"local_path":rec.join("local"),"incoming_hash":uh,"incoming_path":rec.join("incoming"),"live_hash_at_detection":lh,"baseline_path":s.baseline_path,"baseline_source":s.source,"baseline_source_path":s.source_path,"transition":"directory","status":"open"});
        crate::filesystem::atomic(
            &rec.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        let rec_hash = crate::filesystem::hash_dir(&rec)?;
        s.recovery_path = Some(rec.display().to_string());
        conflict_artifact = Some((rec, rec_identity, rec_hash));
        s.status = "conflict".into()
    } else if hash_dir(&local)? != lh {
        s.status = "changed_during_update".into()
    } else {
        replacement = Some(replace_dir_bound(&local, stage.path(), &live_parent)?);
        let (bp, h, baseline) = snapshot_transaction(a, key, &up)?;
        baseline_replacement = Some(baseline);
        s.baseline_path = bp.display().to_string();
        s.baseline_hash = h;
        s.status = "synced".into();
        s.update_count += 1;
        s.last_sync = now()
    }
    a.state.subscriptions.insert(key.into(), s.clone());
    if let Some(replacement) = replacement.as_mut() {
        replacement
            .prepare()
            .context("prepare live replacement commit")?;
    }
    if let Some(baseline) = baseline_replacement.as_mut() {
        if let Err(error) = baseline.prepare() {
            a.state = previous_state.clone();
            return Err(error)
                .context("prepare baseline replacement commit; both replacements rolled back");
        }
    }
    if let Err(error) = a.save() {
        a.state = previous_state.clone();
        if let Some((path, identity, hash)) = conflict_artifact {
            if let Err(cleanup) = remove_owned_directory(&a.recovery, &path, Some(identity), &hash)
            {
                return Err(error).context(format!(
                    "persist update state; recovery retained: {cleanup}"
                ));
            }
        }
        return Err(error).context("persist update state; live and baseline rolled back");
    }
    if let Some(replacement) = replacement.as_mut() {
        replacement.commit()?;
    }
    if let Some(baseline) = baseline_replacement.as_mut() {
        baseline.commit()?;
    }
    let mut output = serde_json::json!({"skill":s.skill,"relationship":key,"status":s.status});
    if let Some(status) = recovered_from(&previous_status) {
        output["recovered_from"] = serde_json::Value::String(status.into());
    }
    Ok(output)
}
pub(crate) fn publish_to_repo(
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
    let tmp_path = crate::filesystem::canonicalize_path_with_missing(tmp.path())
        .context("canonicalize publication repository")?;
    let destination_rel = format!("skills/{skill}");
    let destination_rel_path = Path::new(&destination_rel);
    let key = publication_key(skill, url, &branch_name, &destination_rel);
    assert_no_symlink_path(&tmp_path, destination_rel_path)?;
    let destination = tmp_path.join(destination_rel_path);
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
    let pending_for_destination = a
        .state
        .pending_publications
        .values()
        .filter(|pending| {
            pending.publication.skill == skill && pending.publication.destination == url
        })
        .collect::<Vec<_>>();
    if pending_for_destination.iter().any(|pending| {
        pending.publication.branch != branch_name
            || pending.publication.path != destination_rel
            || !pending.publication.approved
    }) {
        return Err(anyhow!(
            "pending publication intent does not match destination identity"
        ));
    }
    let previously_published = previous
        .and_then(|publication| publication.last_hash.as_ref())
        .is_some_and(|hash| hash == &existing_hash)
        || pending_for_destination.iter().any(|pending| {
            pending.publication.skill == skill
                && pending.publication.destination == url
                && pending.publication.branch == branch_name
                && pending.publication.path == destination_rel
                && pending.publication.approved
        });
    if destination_exists
        && existing_hash != current_hash
        && !existing_is_only_operational
        && !previously_published
    {
        return Err(anyhow!("destination has unexplained modifications"));
    }
    let destination_parent_dir = tmp_path.join("skills");
    fs::create_dir_all(&destination_parent_dir)?;
    let publication_parent = crate::filesystem::open_directory_file_bound(&destination_parent_dir)?;
    let destination_stage_parent = tempfile::tempdir_in(&tmp_path)?;
    let staged_destination = destination_stage_parent.path().join("skill");
    if destination_exists {
        copy_existing_tree(&destination, &staged_destination)?;
    } else {
        fs::create_dir_all(&staged_destination)?;
    }
    let removed = sync_managed_tree(&source_candidate, &staged_destination)?;
    if let Some(parent) = destination.parent() {
        assert_no_symlink_path(&tmp_path, parent.strip_prefix(&tmp_path)?)?;
        fs::create_dir_all(parent)?;
    }
    let mut replacement =
        replace_dir_bound(&destination, &staged_destination, &publication_parent)?;
    for (path, _, _) in &source_files {
        let relative = format!("{destination_rel}/{}", path.to_string_lossy());
        run_git(Some(&tmp_path), &["add", "--", &relative])?;
    }
    for path in &removed {
        let relative = format!("{destination_rel}/{}", path.to_string_lossy());
        run_git(Some(&tmp_path), &["add", "-u", "--", &relative])?;
    }
    if hash_dir(&source)? != current_hash {
        return Err(anyhow!("source changed during publication; retry"));
    }
    let changed = cached_changes(&tmp_path)?;
    if changed {
        run_git(
            Some(&tmp_path),
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
                attempt_count: 0,
                last_attempt_at: 0,
                next_attempt_at: 0,
                last_error_status: None,
            },
        );
        a.save().context("persist publication intent before push")?;
        #[cfg(feature = "test-hooks")]
        if std::env::var("SKILLSYNC_TEST_FAIL_PUSH_ONCE").as_deref() == Ok(skill) {
            std::env::remove_var("SKILLSYNC_TEST_FAIL_PUSH_ONCE");
            return Err(anyhow!("injected publication push failure (test-only)"));
        }
        run_git(Some(&tmp_path), &["push", "origin", &branch_name])?;
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
    let local_hash = run_git(Some(&tmp_path), &["rev-parse", "HEAD"])?;
    if remote_hash != local_hash {
        return Err(anyhow!("remote readback did not match published commit"));
    }
    if hash_dir(&source)? != current_hash {
        return Err(anyhow!("source changed after publication; retry"));
    }
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_MUTATE_SOURCE_AFTER_READBACK").as_deref() == Ok(skill) {
        std::fs::write(source.join("SKILL.md"), format!("name: {skill}\\nv2\\n"))
            .context("injected source mutation after final pre-state check (test-only)")?;
        std::env::remove_var("SKILLSYNC_TEST_MUTATE_SOURCE_AFTER_READBACK");
    }
    if hash_dir(&source)? != current_hash {
        return Err(anyhow!(
            "source changed before publication state commit; retry"
        ));
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
    replacement.prepare_publication(skill)?;
    replacement.commit_publication(skill)?;
    let previous_state = a.state.clone();
    a.state.pending_publications.remove(&key);
    a.state.publications.insert(key, publication);
    if let Err(error) = save_publication_final_state(a, skill) {
        a.state = previous_state;
        return Err(error);
    }
    Ok(serde_json::json!({"skill":skill,"status":"published"}))
}

fn save_publication_final_state(a: &App, skill: &str) -> Result<()> {
    #[cfg(not(feature = "test-hooks"))]
    let _ = skill;
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_FAIL_STATE_SAVE_AFTER_PUSH").as_deref() == Ok(skill) {
        std::env::remove_var("SKILLSYNC_TEST_FAIL_STATE_SAVE_AFTER_PUSH");
        return Err(anyhow!(
            "injected post-push publication state-save failure (test-only)"
        ));
    }
    a.save()
}
