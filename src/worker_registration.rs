use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::filesystem;

const VERSION: u32 = 1;
#[cfg(target_os = "linux")]
const SERVICE: &str = "skillsync-worker.service";

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Registration {
    version: u32,
    platform: String,
    service_identity: String,
    executable_path: String,
    executable_hash: String,
    interval: u64,
    registration_path: String,
    enabled: bool,
    registration_hash: String,
}

fn metadata_path(config: &Path) -> PathBuf {
    config.join("worker-registration.json")
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("home directory unavailable"))
}
fn paths() -> Result<(PathBuf, PathBuf, String)> {
    #[cfg(target_os = "linux")]
    {
        let root = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".local/share"))
            .join("skillsync");
        return Ok((
            root.join("bin").join("skillsync"),
            home()?.join(".config/systemd/user/skillsync-worker.service"),
            SERVICE.into(),
        ));
    }
    #[cfg(target_os = "macos")]
    {
        let root = home()?.join("Library/Application Support/skillsync");
        return Ok((
            root.join("bin").join("skillsync"),
            home()?.join("Library/LaunchAgents/com.skillsync.worker.plist"),
            "com.skillsync.worker".into(),
        ));
    }
    #[cfg(target_os = "windows")]
    {
        let root = PathBuf::from(
            std::env::var_os("LOCALAPPDATA").ok_or_else(|| anyhow!("LOCALAPPDATA unavailable"))?,
        )
        .join("skillsync");
        return Ok((
            root.join("bin").join("skillsync.exe"),
            root.join("worker-task.xml"),
            "Skillsync Worker".into(),
        ));
    }
    #[allow(unreachable_code)]
    Err(anyhow!("worker registration unsupported on this platform"))
}
fn hash(path: &Path) -> Result<String> {
    let mut h = Sha256::new();
    h.update(fs::read(path)?);
    Ok(format!("{:x}", h.finalize()))
}
fn regular_owned(path: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() || !m.is_file() {
        return Err(anyhow!(
            "regular non-symlink file required: {}",
            path.display()
        ));
    }
    Ok(m)
}
fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    filesystem::atomic(path, data)
}

#[cfg(target_os = "linux")]
pub(crate) fn render_linux_unit(executable: &Path, interval: u64) -> Result<String> {
    let p = executable
        .to_str()
        .ok_or_else(|| anyhow!("executable path is not UTF-8"))?;
    if p.chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        return Err(anyhow!("unsafe executable path for systemd"));
    }
    Ok(format!("[Unit]\nDescription=Skillsync worker\n\n[Service]\nType=simple\nExecStart={p} worker --interval {interval}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"))
}
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
#[cfg(target_os = "macos")]
fn render_macos_plist(executable: &Path, interval: u64) -> Result<String> {
    let p = xml_escape(
        executable
            .to_str()
            .ok_or_else(|| anyhow!("executable path is not UTF-8"))?,
    );
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Label</key><string>com.skillsync.worker</string><key>ProgramArguments</key><array><string>{p}</string><string>worker</string><string>--interval</string><string>{interval}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>
"#,
    ))
}
#[cfg(target_os = "windows")]
fn render_windows_task(executable: &Path, interval: u64) -> Result<String> {
    let p = xml_escape(
        executable
            .to_str()
            .ok_or_else(|| anyhow!("executable path is not UTF-8"))?,
    );
    Ok(format!(
        r#"<?xml version="1.0"?><Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers><Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Actions Context="Author"><Exec><Command>{p}</Command><Arguments>worker --interval {interval}</Arguments></Exec></Actions></Task>"#,
    ))
}
fn expected(config: &Path) -> Result<(PathBuf, PathBuf, String)> {
    let (e, r, i) = paths()?;
    let metadata_is_symlink = fs::symlink_metadata(metadata_path(config))
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !e.is_absolute() || !r.is_absolute() || metadata_is_symlink {
        return Err(anyhow!("worker registration paths are unsafe"));
    }
    Ok((e, r, i))
}
fn load(config: &Path) -> Result<Option<Registration>> {
    let p = metadata_path(config);
    if !present(&p)? {
        return Ok(None);
    };
    regular_owned(&p)?;
    Ok(Some(serde_json::from_slice(&fs::read(p)?)?))
}
fn validate(config: &Path, r: &Registration) -> Result<(PathBuf, PathBuf, String)> {
    let (exe, reg, id) = expected(config)?;
    if r.version != VERSION
        || r.platform != std::env::consts::OS
        || r.service_identity != id
        || r.executable_path != exe.to_string_lossy()
        || r.registration_path != reg.to_string_lossy()
        || r.interval == 0
        || r.executable_hash.len() != 64
        || !r.executable_hash.bytes().all(|b| b.is_ascii_hexdigit())
        || r.registration_hash.len() != 64
        || !r.registration_hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(anyhow!(
            "worker registration metadata is invalid or does not belong to this installation"
        ));
    }
    if present(&reg)? {
        regular_owned(&reg)?;
        let mut h = Sha256::new();
        h.update(fs::read(&reg)?);
        if r.registration_hash != format!("{:x}", h.finalize()) {
            return Err(anyhow!("registration artifact ownership validation failed"));
        }
    } else if r.enabled {
        return Err(anyhow!("registration artifact missing"));
    }
    Ok((exe, reg, id))
}
#[derive(Debug, Default, Clone, Copy)]
struct ProviderInstallState {
    written: bool,
    activation_attempted: bool,
}

fn registration_bytes(exe: &Path, interval: u64) -> Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        return Ok(render_linux_unit(exe, interval)?.into_bytes());
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(render_macos_plist(exe, interval)?.into_bytes());
    }
    #[cfg(target_os = "windows")]
    {
        return Ok(render_windows_task(exe, interval)?.into_bytes());
    }
    #[allow(unreachable_code)]
    Err(anyhow!("worker registration unsupported on this platform"))
}
fn provider_install(
    exe: &Path,
    reg: &Path,
    interval: u64,
    identity: &str,
    registration: &[u8],
    state: &mut ProviderInstallState,
) -> Result<()> {
    let _ = (exe, interval, identity);
    #[cfg(feature = "test-hooks")]
    if let Ok(v) = std::env::var("SKILLSYNC_TEST_WORKER_PROVIDER") {
        if v == "fail" {
            return Err(anyhow!("injected worker provider failure"));
        }
        if v == "ok" {
            write_private(reg, registration)?;
            state.written = true;
            state.activation_attempted = true;
            return Ok(());
        }
    }
    #[cfg(target_os = "linux")]
    {
        write_private(reg, registration)?;
        state.written = true;
        let reload = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()?;
        if !reload.success() {
            return Err(anyhow!("systemctl provider operation failed"));
        }
        state.activation_attempted = true;
        let enable = Command::new("systemctl")
            .args(["--user", "enable", "--now", identity])
            .status()?;
        if !enable.success() {
            return Err(anyhow!("systemctl provider operation failed"));
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        write_private(reg, registration)?;
        state.written = true;
        let reg_arg = reg
            .to_str()
            .ok_or_else(|| anyhow!("registration path is not UTF-8"))?;
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        state.activation_attempted = true;
        if !Command::new("launchctl")
            .args(["bootstrap", &domain, reg_arg])
            .status()?
            .success()
        {
            return Err(anyhow!("launchctl provider operation failed"));
        }
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        write_private(reg, registration)?;
        state.written = true;
        let reg_arg = reg
            .to_str()
            .ok_or_else(|| anyhow!("registration path is not UTF-8"))?;
        state.activation_attempted = true;
        if !Command::new("schtasks")
            .args(["/Create", "/TN", identity, "/XML", reg_arg, "/F"])
            .status()?
            .success()
        {
            return Err(anyhow!("schtasks provider operation failed"));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err(anyhow!("worker registration unsupported on this platform"))
}
const PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

fn bounded_provider_status(mut command: Command) -> std::io::Result<ExitStatus> {
    let mut child: Child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + PROVIDER_PROBE_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "worker provider probe timed out",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn provider_state(identity: &str) -> &'static str {
    #[cfg(feature = "test-hooks")]
    if let Ok(value) = std::env::var("SKILLSYNC_TEST_WORKER_PROVIDER") {
        return match value.as_str() {
            "ok" => "active",
            "inactive" => "inactive",
            "unavailable" => "unavailable",
            _ => "unknown",
        };
    }

    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("systemctl");
        command.args(["--user", "is-active", identity]);
        let output = bounded_provider_status(command);
        return match output {
            Ok(status) if status.success() => "active",
            Ok(status) if status.code() == Some(3) => "inactive",
            Ok(_) => "unknown",
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "unavailable",
            Err(_) => "unknown",
        };
    }
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let target = format!("{domain}/{identity}");
        let mut command = Command::new("launchctl");
        command.args(["print", &target]);
        let output = bounded_provider_status(command);
        return match output {
            Ok(status) if status.success() => "active",
            Ok(_) => "unknown",
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "unavailable",
            Err(_) => "unknown",
        };
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("schtasks");
        command.args(["/Query", "/TN", identity, "/FO", "LIST", "/NH"]);
        let output = bounded_provider_status(command);
        return match output {
            Ok(status) if status.success() => "active",
            Ok(_) => "unknown",
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "unavailable",
            Err(_) => "unknown",
        };
    }
    #[allow(unreachable_code)]
    "unavailable"
}

fn provider_disable(identity: &str, reg: &Path) -> Result<()> {
    let _ = reg;
    #[cfg(feature = "test-hooks")]
    if let Ok(v) = std::env::var("SKILLSYNC_TEST_WORKER_PROVIDER") {
        if v == "fail" {
            return Err(anyhow!("injected worker provider failure"));
        }
        if v == "ok" {
            return Ok(());
        }
    }
    #[cfg(target_os = "linux")]
    {
        let s = Command::new("systemctl")
            .args(["--user", "disable", "--now", identity])
            .status()?;
        if !s.success() {
            return Err(anyhow!("systemctl provider operation failed"));
        }
        let _ = reg;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        if !Command::new("launchctl")
            .args(["bootout", &domain, identity])
            .status()?
            .success()
        {
            return Err(anyhow!("launchctl provider operation failed"));
        }
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let _ = reg;
        if !Command::new("schtasks")
            .args(["/Delete", "/TN", identity, "/F"])
            .status()?
            .success()
        {
            return Err(anyhow!("schtasks provider operation failed"));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err(anyhow!("worker registration unsupported on this platform"))
}
fn install_executable(current: &Path, target: &Path) -> Result<bool> {
    if present(target)? {
        regular_owned(target)?;
        if hash(target)? != hash(current)? {
            return Err(anyhow!(
                "installed worker executable changed; refusing replacement"
            ));
        }
        return Ok(false);
    }
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("worker executable has no parent"))?;
    filesystem::assert_no_symlink_path(parent, Path::new("."))?;
    fs::create_dir_all(parent)?;
    filesystem::assert_no_symlink_path(parent, Path::new("."))?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    f.write_all(&fs::read(current)?)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
    }
    Ok(true)
}

fn rollback_owned(
    exe: &Path,
    reg: &Path,
    exe_created: bool,
    old_reg: Option<&[u8]>,
    registration_written: bool,
    expected_reg: &[u8],
    expected_exe_hash: &str,
) -> Result<()> {
    if registration_written {
        if !present(reg)? {
            return Err(anyhow!("registration artifact missing; recovery required"));
        }
        regular_owned(reg)?;
        if fs::read(reg)? != expected_reg {
            return Err(anyhow!(
                "registration artifact ownership changed; recovery required"
            ));
        }
        if let Some(bytes) = old_reg {
            write_private(reg, bytes)?;
        } else if old_reg.is_none() {
            fs::remove_file(reg)?;
        }
    } else if let Some(bytes) = old_reg {
        if !present(reg)? {
            return Err(anyhow!("registration artifact missing; recovery required"));
        }
        regular_owned(reg)?;
        if fs::read(reg)? != bytes {
            return Err(anyhow!(
                "registration artifact changed before provider write; recovery required"
            ));
        }
    } else if present(reg)? {
        return Err(anyhow!(
            "unexpected registration artifact appeared; recovery required"
        ));
    }
    if exe_created && present(exe)? {
        regular_owned(exe)?;
        if hash(exe)? != expected_exe_hash {
            return Err(anyhow!(
                "worker executable ownership changed; recovery required"
            ));
        }
        fs::remove_file(exe)?;
    }
    Ok(())
}
fn save_metadata(config: &Path, registration: &Registration) -> Result<()> {
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_FAIL_REGISTRATION_METADATA_SAVE").as_deref() == Ok("1") {
        return Err(anyhow!("injected registration metadata-save failure"));
    }
    write_private(
        &metadata_path(config),
        &serde_json::to_vec_pretty(registration)?,
    )
}
pub(crate) fn enable(config: &Path, interval: u64) -> Result<serde_json::Value> {
    if interval == 0 {
        return Err(anyhow!("worker interval must be greater than zero seconds"));
    };
    let (exe, reg, id) = expected(config)?;
    let current = std::env::current_exe()?;
    let old = load(config)?;
    if let Some(old) = old.as_ref() {
        let (oe, or, _) = validate(config, old)?;
        if old.enabled {
            regular_owned(&oe)?;
            if hash(&oe)? != old.executable_hash {
                return Err(anyhow!("worker executable ownership validation failed"));
            };
            if !or.try_exists()? {
                return Err(anyhow!("registration artifact missing"));
            };
            regular_owned(&or)?;
            if old.interval == interval {
                return Ok(serde_json::json!({"worker":"already_enabled","interval":interval}));
            }
            return Err(anyhow!(
                "worker already enabled; disable first to change interval"
            ));
        }
    }
    if old.is_none() && present(&reg)? {
        return Err(anyhow!(
            "unowned worker registration artifact exists; refusing replacement"
        ));
    }
    let exe_created = install_executable(&current, &exe)?;
    let old_reg = if present(&reg)? {
        regular_owned(&reg)?;
        Some(fs::read(&reg)?)
    } else {
        None
    };
    let expected_exe_hash = hash(&exe)?;
    let record = Registration {
        version: VERSION,
        platform: std::env::consts::OS.into(),
        service_identity: id.clone(),
        executable_path: exe.to_string_lossy().into(),
        executable_hash: expected_exe_hash.clone(),
        interval,
        registration_path: reg.to_string_lossy().into(),
        enabled: true,
        registration_hash: String::new(),
    };
    let registration_bytes = registration_bytes(&exe, interval)?;
    let mut install_state = ProviderInstallState::default();
    if let Err(e) = provider_install(
        &exe,
        &reg,
        interval,
        &id,
        &registration_bytes,
        &mut install_state,
    ) {
        let deactivate = if install_state.activation_attempted {
            provider_disable(&id, &reg)
        } else {
            Ok(())
        };
        let cleanup = rollback_owned(
            &exe,
            &reg,
            exe_created,
            old_reg.as_deref(),
            install_state.written,
            &registration_bytes,
            &expected_exe_hash,
        );
        return match (deactivate, cleanup) {
            (Ok(()), Ok(())) => Err(e).context("register worker; rolled back owned artifacts"),
            (Err(d), _) => Err(e).context(format!(
                "register worker; recovery required: provider deactivation failed: {d}"
            )),
            (_, Err(c)) => Err(e).context(format!(
                "register worker; recovery required: artifact cleanup failed: {c}"
            )),
        };
    }
    let mut record = record;
    let mut rh = Sha256::new();
    rh.update(fs::read(&reg)?);
    record.registration_hash = format!("{:x}", rh.finalize());
    if let Err(e) = save_metadata(config, &record) {
        let disable = if install_state.activation_attempted {
            provider_disable(&id, &reg)
        } else {
            Ok(())
        };
        let cleanup = rollback_owned(
            &exe,
            &reg,
            exe_created,
            old_reg.as_deref(),
            install_state.written,
            &registration_bytes,
            &expected_exe_hash,
        );
        return match (disable, cleanup) {
            (Ok(()), Ok(())) => {
                Err(e).context("save registration metadata; rolled back owned artifacts")
            }
            (Err(d), _) => Err(e).context(format!(
                "save registration metadata; recovery required: provider deactivation failed: {d}"
            )),
            (_, Err(c)) => Err(e).context(format!(
                "save registration metadata; recovery required: artifact cleanup failed: {c}"
            )),
        };
    }
    Ok(serde_json::json!({
        "worker": "enabled",
        "interval": interval,
        "executable": exe,
        "registration": reg
    }))
}
fn owned_bytes(path: &Path, label: &str) -> Result<Vec<u8>> {
    regular_owned(path).with_context(|| format!("validate {label}"))?;
    Ok(fs::read(path)?)
}
fn ensure_owned_bytes(path: &Path, expected: &[u8], label: &str) -> Result<()> {
    if !present(path)? {
        return Err(anyhow!("{label} is missing; recovery required"));
    }
    regular_owned(path)?;
    if fs::read(path)? != expected {
        return Err(anyhow!("{label} ownership changed; recovery required"));
    }
    Ok(())
}
fn restore_if_missing(path: &Path, expected: &[u8], executable: bool, label: &str) -> Result<()> {
    let _ = executable;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(anyhow!(
                    "{label} replacement is not a regular file; recovery required"
                ));
            }
            if fs::read(path)? != expected {
                return Err(anyhow!("{label} replacement changed; recovery required"));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_private(path, expected)?;
            #[cfg(unix)]
            if executable {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
fn remove_owned(path: &Path, expected: &[u8], label: &str) -> Result<()> {
    ensure_owned_bytes(path, expected, label)?;
    #[cfg(feature = "test-hooks")]
    if std::env::var("SKILLSYNC_TEST_FAIL_REGISTRATION_REMOVE").as_deref() == Ok(label) {
        return Err(anyhow!("injected {label} removal failure"));
    }
    fs::remove_file(path)?;
    Ok(())
}
fn provider_activate_existing(
    exe: &Path,
    reg: &Path,
    interval: u64,
    identity: &str,
    registration: &[u8],
    state: &mut ProviderInstallState,
) -> Result<()> {
    let _ = (exe, interval, identity);
    ensure_owned_bytes(reg, registration, "registration artifact")?;
    #[cfg(feature = "test-hooks")]
    if let Ok(v) = std::env::var("SKILLSYNC_TEST_WORKER_PROVIDER") {
        if v == "fail" {
            return Err(anyhow!("injected worker provider failure"));
        }
        if v == "ok" {
            state.activation_attempted = true;
            return Ok(());
        }
    }
    #[cfg(target_os = "linux")]
    {
        let reload = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()?;
        if !reload.success() {
            return Err(anyhow!("systemctl provider operation failed"));
        }
        ensure_owned_bytes(reg, registration, "registration artifact")?;
        state.activation_attempted = true;
        if !Command::new("systemctl")
            .args(["--user", "enable", "--now", identity])
            .status()?
            .success()
        {
            return Err(anyhow!("systemctl provider operation failed"));
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let reg_arg = reg
            .to_str()
            .ok_or_else(|| anyhow!("registration path is not UTF-8"))?;
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        ensure_owned_bytes(reg, registration, "registration artifact")?;
        state.activation_attempted = true;
        if !Command::new("launchctl")
            .args(["bootstrap", &domain, reg_arg])
            .status()?
            .success()
        {
            return Err(anyhow!("launchctl provider operation failed"));
        }
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        let reg_arg = reg
            .to_str()
            .ok_or_else(|| anyhow!("registration path is not UTF-8"))?;
        ensure_owned_bytes(reg, registration, "registration artifact")?;
        state.activation_attempted = true;
        if !Command::new("schtasks")
            .args(["/Create", "/TN", identity, "/XML", reg_arg, "/F"])
            .status()?
            .success()
        {
            return Err(anyhow!("schtasks provider operation failed"));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    Err(anyhow!("worker registration unsupported on this platform"))
}
fn restore_provider(
    exe: &Path,
    reg: &Path,
    interval: u64,
    identity: &str,
    registration: &[u8],
) -> Result<()> {
    let mut state = ProviderInstallState::default();
    provider_activate_existing(exe, reg, interval, identity, registration, &mut state)?;
    if !state.activation_attempted {
        return Err(anyhow!("provider restoration did not activate the worker"));
    }
    ensure_owned_bytes(reg, registration, "registration artifact")
}
fn restore_metadata_after_failed_save(
    path: &Path,
    previous: &[u8],
    attempted: &[u8],
) -> Result<()> {
    if !present(path)? {
        return Err(anyhow!(
            "registration metadata is missing; recovery required"
        ));
    }
    regular_owned(path)?;
    let current = fs::read(path)?;
    if current == previous {
        return Ok(());
    }
    if current != attempted {
        return Err(anyhow!(
            "registration metadata changed externally; recovery required"
        ));
    }
    write_private(path, previous)?;
    ensure_owned_bytes(path, previous, "registration metadata")
}
fn rollback_uninstall(
    exe: &Path,
    reg: &Path,
    metadata: &Path,
    exe_bytes: &[u8],
    reg_bytes: Option<&[u8]>,
    metadata_bytes: &[u8],
    registration: &Registration,
) -> Result<()> {
    restore_if_missing(exe, exe_bytes, true, "worker executable")?;
    if let Some(bytes) = reg_bytes {
        restore_if_missing(reg, bytes, false, "registration artifact")?;
    }
    restore_if_missing(metadata, metadata_bytes, false, "registration metadata")?;
    if registration.enabled {
        let bytes = reg_bytes.ok_or_else(|| anyhow!("enabled worker registration missing"))?;
        restore_provider(
            exe,
            reg,
            registration.interval,
            &registration.service_identity,
            bytes,
        )?;
    }
    Ok(())
}
pub(crate) fn disable(config: &Path) -> Result<serde_json::Value> {
    let Some(r) = load(config)? else {
        return Ok(serde_json::json!({"worker":"already_absent"}));
    };
    let (exe, reg, id) = validate(config, &r)?;
    regular_owned(&exe)?;
    if hash(&exe)? != r.executable_hash {
        return Err(anyhow!("worker executable ownership validation failed"));
    }
    if !r.enabled {
        return Ok(serde_json::json!({"worker":"already_disabled"}));
    }
    let metadata = metadata_path(config);
    let previous_metadata = owned_bytes(&metadata, "registration metadata")?;
    let registration = owned_bytes(&reg, "registration artifact")?;
    let mut disabled = r.clone();
    disabled.enabled = false;
    let attempted_metadata = serde_json::to_vec_pretty(&disabled)?;

    provider_disable(&id, &reg)?;
    if let Err(save_error) = save_metadata(config, &disabled) {
        let metadata_restore =
            restore_metadata_after_failed_save(&metadata, &previous_metadata, &attempted_metadata);
        let registration_owned = ensure_owned_bytes(&reg, &registration, "registration artifact");
        let provider_restore = if metadata_restore.is_ok() && registration_owned.is_ok() {
            restore_provider(&exe, &reg, r.interval, &id, &registration)
        } else {
            Err(anyhow!(
                "owned state could not be proven for provider restoration"
            ))
        };
        return match (metadata_restore, registration_owned, provider_restore) {
            (Ok(()), Ok(()), Ok(())) => Err(save_error)
                .context("disable worker; rolled back provider and metadata"),
            (metadata_error, registration_error, provider_error) => Err(save_error).context(
                format!(
                    "disable worker; recovery required: metadata={:?}, registration={:?}, provider={:?}",
                    metadata_error.err(),
                    registration_error.err(),
                    provider_error.err()
                ),
            ),
        };
    }
    Ok(serde_json::json!({"worker":"disabled"}))
}
pub(crate) fn status(config: &Path) -> Result<serde_json::Value> {
    let Some(r) = load(config)? else {
        return Ok(serde_json::json!({"registered":false,"enabled":false}));
    };
    let (exe, reg, id) = validate(config, &r)?;
    regular_owned(&exe)?;
    if hash(&exe)? != r.executable_hash {
        return Err(anyhow!("worker executable ownership validation failed"));
    };
    if r.enabled && !present(&reg)? {
        return Err(anyhow!("active worker registration artifact is missing"));
    };
    if present(&reg)? {
        regular_owned(&reg)?;
    }
    let provider_state = if r.enabled {
        provider_state(&id)
    } else {
        "inactive"
    };
    Ok(
        serde_json::json!({"registered":true,"enabled":r.enabled,"provider_state":provider_state,"interval":r.interval,"executable":exe,"registration":reg}),
    )
}
pub(crate) fn uninstall(config: &Path) -> Result<serde_json::Value> {
    let Some(r) = load(config)? else {
        return Ok(serde_json::json!({"worker":"already_absent"}));
    };
    let (exe, reg, id) = validate(config, &r)?;
    regular_owned(&exe)?;
    if hash(&exe)? != r.executable_hash {
        return Err(anyhow!("worker executable ownership validation failed"));
    }
    let metadata = metadata_path(config);
    let metadata_bytes = owned_bytes(&metadata, "registration metadata")?;
    let exe_bytes = owned_bytes(&exe, "worker executable")?;
    let reg_bytes = if present(&reg)? {
        Some(owned_bytes(&reg, "registration artifact")?)
    } else {
        None
    };
    if r.enabled && reg_bytes.is_none() {
        return Err(anyhow!("enabled worker registration missing"));
    }
    if r.enabled {
        provider_disable(&id, &reg)?;
    }
    let removal = (|| {
        if let Some(bytes) = reg_bytes.as_deref() {
            remove_owned(&reg, bytes, "registration artifact")?;
        }
        remove_owned(&exe, &exe_bytes, "worker executable")?;
        remove_owned(&metadata, &metadata_bytes, "registration metadata")?;
        Ok::<(), anyhow::Error>(())
    })();
    if let Err(removal_error) = removal {
        let rollback = rollback_uninstall(
            &exe,
            &reg,
            &metadata,
            &exe_bytes,
            reg_bytes.as_deref(),
            &metadata_bytes,
            &r,
        );
        return match rollback {
            Ok(()) => Err(removal_error).context("uninstall worker; rolled back"),
            Err(rollback_error) => Err(removal_error).context(format!(
                "uninstall worker; recovery required: rollback failed: {rollback_error}"
            )),
        };
    }
    Ok(serde_json::json!({"worker":"uninstalled"}))
}
