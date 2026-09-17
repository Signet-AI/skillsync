use anyhow::{anyhow, Context, Result};
use std::{
    fs,
    io::{IsTerminal, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

struct OwnedTemp {
    path: PathBuf,
    identity: fs::Metadata,
}

impl OwnedTemp {
    fn new(path: PathBuf, file: &fs::File) -> Result<Self> {
        Ok(Self {
            path,
            identity: file.metadata()?,
        })
    }
}

impl Drop for OwnedTemp {
    fn drop(&mut self) {
        let Ok(current) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if current.file_type().is_symlink()
            || !current.is_file()
            || !same_object(&current, &self.identity)
        {
            return;
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn same_object(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        a.volume_serial_number() == b.volume_serial_number()
            && a.file_index() == b.file_index()
    }
    #[cfg(not(any(unix, windows)))]
    {
        a.len() == b.len() && a.modified().ok() == b.modified().ok()
    }
}

fn safe_temp_path(dir: &Path) -> Result<(PathBuf, fs::File)> {
    for attempt in 0..32u32 {
        let path = dir.join(format!(
            ".config.toml.edit-{}-{}",
            std::process::id(),
            super::unique_stamp() + attempt as u128
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = fs::OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .mode(0o600)
                .open(&path);
            match file {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        #[cfg(not(unix))]
        {
            match fs::OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Err(anyhow!("could not create a unique temporary config file"))
}

#[cfg(unix)]
#[allow(dead_code)]
fn publish_anchored(
    directory: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
    temp: &Path,
    replace: bool,
) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let src = CString::new(
        temp.file_name()
            .ok_or_else(|| anyhow!("temporary config file has no name"))?
            .as_bytes(),
    )?;
    let dst = CString::new(name.as_bytes())?;
    let rc = if replace {
        unsafe { libc::renameat(directory, src.as_ptr(), directory, dst.as_ptr()) }
    } else {
        unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                directory,
                src.as_ptr(),
                directory,
                dst.as_ptr(),
                libc::RENAME_NOREPLACE,
            ) as libc::c_int
        }
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(unix)]
fn publish_missing_from_handle(
    directory: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
    file: &fs::File,
) -> Result<()> {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, io::AsRawFd},
    };
    let dst = CString::new(name.as_bytes())?;
    let empty = CString::new("")?;
    let rc = unsafe {
        libc::linkat(
            file.as_raw_fd(),
            empty.as_ptr(),
            directory,
            dst.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[allow(unreachable_code)]
pub(crate) fn edit_config(a: &super::App, json: bool) -> Result<serde_json::Value> {
    if json {
        return Err(anyhow!("config edit cannot run with --json"));
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(anyhow!("config edit requires an interactive terminal"));
    }
    let path = a.config.join("config.toml");
    super::assert_no_symlink_path(&a.config, Path::new("."))?;
    let config_identity = fs::metadata(&a.config)?;
    #[cfg(windows)]
    let config_directory_identity = super::windows_path_identity(&a.config)?;
    #[cfg(unix)]
    let directory = {
        use std::os::fd::FromRawFd;
        let fd = super::open_directory_fd(&a.config)?;
        unsafe { fs::File::from_raw_fd(fd) }
    };
    let original = match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
            return Err(anyhow!(
                "config path is not a regular file: {}",
                path.display()
            ))
        }
        Ok(m) => Some((m, super::read_regular_file(&path, None)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let editor = std::env::var_os("VISUAL")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("EDITOR").filter(|v| !v.is_empty()))
        .ok_or_else(|| anyhow!("no editor configured; set VISUAL or EDITOR"))?;
    let default = format!("library = {:?}\n", a.library.display().to_string());
    let (temp, mut file) = safe_temp_path(&a.config)?;
    let _owned = OwnedTemp::new(temp.clone(), &file)?;
    let result = (|| {
        file.write_all(
            original
                .as_ref()
                .map(|(_, b)| b.as_slice())
                .unwrap_or(default.as_bytes()),
        )?;
        file.sync_all()?;
        let temp_identity = file.metadata()?;
        drop(file);
        let status = Command::new(&editor)
            .arg(&temp)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("launch editor {}", PathBuf::from(&editor).display()))?;
        if !status.success() {
            return Err(anyhow!("editor exited unsuccessfully: {status}"));
        }
        // Validate one concrete editor result, then immediately prove that its
        // pathname still denotes the same object and bytes. Editors may rewrite
        // in place, so identity alone is not sufficient.
        let edited = super::read_regular_file(&temp, Some(&temp_identity))?;
        validate_config_bytes(&edited)?;
        verify_temp_unchanged(&temp, &temp_identity, &edited)?;
        // Never publish from the editor-owned pathname. Stage the exact bytes
        // that were validated into a second operation-owned file.
        let (staged, mut staged_file) = safe_temp_path(&a.config)?;
        let _staged_owned = OwnedTemp::new(staged.clone(), &staged_file)?;
        staged_file.write_all(&edited)?;
        staged_file.sync_all()?;
        staged_file.seek(std::io::SeekFrom::Start(0))?;
        let mut staged_bytes = Vec::new();
        staged_file.read_to_end(&mut staged_bytes)?;
        if staged_bytes != edited {
            return Err(anyhow!("publication staging bytes changed"));
        }
        // Keep the validated staging handle alive through publication; do not
        // reopen a pathname that another process can replace.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let retained = directory.metadata()?;
            if retained.dev() != config_identity.dev() || retained.ino() != config_identity.ino() {
                return Err(anyhow!(
                    "config directory changed while it was being edited"
                ));
            }
            if let Some((original_metadata, original_bytes)) = &original {
                let mut target = super::open_child_file(&a.config, "config.toml", false)?;
                let target_metadata = target.metadata()?;
                use std::os::unix::fs::MetadataExt;
                verify_original_unchanged(
                    &path,
                    &(original_metadata.clone(), original_bytes.clone()),
                )?;
                if target_metadata.dev() != original_metadata.dev()
                    || target_metadata.ino() != original_metadata.ino()
                {
                    return Err(anyhow!("config path changed while it was being edited"));
                }
                let mut current = Vec::new();
                target.read_to_end(&mut current)?;
                if current != *original_bytes {
                    return Err(anyhow!("config changed while it was being edited"));
                }
                let publish_result = (|| -> Result<()> {
                    target.set_len(0)?;
                    target.rewind()?;
                    #[cfg(feature = "test-hooks")]
                    if std::env::var_os("SKILLSYNC_TEST_CONFIG_PUBLISH_FAILURE").is_some() {
                        std::env::remove_var("SKILLSYNC_TEST_CONFIG_PUBLISH_FAILURE");
                        return Err(anyhow!("injected config publication failure"));
                    }
                    target.write_all(&edited)?;
                    target.sync_all()?;
                    Ok(())
                })();
                if let Err(error) = publish_result {
                    if let Err(recovery) = restore_target(&mut target, original_bytes) {
                        return Err(anyhow!("recovery required: {error}; restoring original config failed: {recovery}"));
                    }
                    return Err(error);
                }
                verify_original_unchanged(&path, &(original_metadata.clone(), edited.clone()))
                    .map_err(|error| anyhow!("config publication target changed: {error}"))?;
            }
        }
        #[cfg(windows)]
        {
            if super::windows_path_identity(&a.config)? != config_directory_identity {
                return Err(anyhow!(
                    "config directory changed while it was being edited"
                ));
            }
            if let Some((original_metadata, original_bytes)) = &original {
                verify_original_unchanged(
                    &path,
                    &(original_metadata.clone(), original_bytes.clone()),
                )?;
                let mut target = super::open_regular_file_bound(&path, true, false)?;
                let mut current = Vec::new();
                target.read_to_end(&mut current)?;
                if current != *original_bytes {
                    return Err(anyhow!("config changed while it was being edited"));
                }
                let publish_result = (|| -> Result<()> {
                    target.set_len(0)?;
                    target.rewind()?;
                    #[cfg(feature = "test-hooks")]
                    if std::env::var_os("SKILLSYNC_TEST_CONFIG_PUBLISH_FAILURE").is_some() {
                        std::env::remove_var("SKILLSYNC_TEST_CONFIG_PUBLISH_FAILURE");
                        return Err(anyhow!("injected config publication failure"));
                    }
                    target.write_all(&edited)?;
                    target.sync_all()?;
                    target.rewind()?;
                    let mut verified = Vec::new();
                    target.read_to_end(&mut verified)?;
                    if verified != edited {
                        return Err(anyhow!("published config bytes do not match the editor result"));
                    }
                    Ok(())
                })();
                if let Err(error) = publish_result {
                    if let Err(recovery) = restore_target(&mut target, original_bytes) {
                        return Err(anyhow!("recovery required: {error}; restoring original config failed: {recovery}"));
                    }
                    return Err(error);
                }
                verify_original_unchanged(&path, &(original_metadata.clone(), edited.clone()))
                    .map_err(|error| anyhow!("config publication target changed: {error}"))?;
            } else {
                let mut target = super::open_regular_file_bound(&path, true, true)?;
                let publication = (|| -> Result<()> {
                    target.write_all(&edited)?;
                    target.sync_all()?;
                    target.rewind()?;
                    let mut verified = Vec::new();
                    target.read_to_end(&mut verified)?;
                    if verified != edited {
                        return Err(anyhow!("published config bytes do not match the editor result"));
                    }
                    Ok(())
                })();
                if let Err(error) = publication {
                    drop(target);
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = config_identity;
            return Err(anyhow!("native config publication is unavailable on this platform"));
        }
        #[cfg(unix)]
        if original.is_none() {
            use std::os::fd::AsRawFd;
            publish_missing_from_handle(
                directory.as_raw_fd(),
                path.file_name().unwrap(),
                &staged_file,
            )?;
        }
        Ok(())
    })();
    result?;
    Ok(serde_json::json!({"path": path}))
}

fn verify_temp_unchanged(path: &Path, before: &fs::Metadata, expected: &[u8]) -> Result<()> {
    let after = fs::symlink_metadata(path)?;
    if after.file_type().is_symlink() || !after.is_file() {
        return Err(anyhow!(
            "temporary config file changed while it was being edited"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(anyhow!(
                "temporary config file changed while it was being edited"
            ));
        }
    }
    let current = super::read_regular_file(path, Some(before))?;
    if current != expected {
        return Err(anyhow!(
            "temporary config file changed while it was being edited"
        ));
    }
    Ok(())
}

fn validate_config_bytes(bytes: &[u8]) -> Result<()> {
    let text = String::from_utf8(bytes.to_vec()).context("config.toml is not valid UTF-8")?;
    let config = toml::from_str::<super::FileConfig>(&text).context("invalid config.toml")?;
    let library = super::effective_library_path(config.library.map(PathBuf::from));
    super::resolve_library_path(&library).context("invalid effective config library")?;
    Ok(())
}

fn verify_original_unchanged(path: &Path, original: &(fs::Metadata, Vec<u8>)) -> Result<()> {
    let current_metadata = fs::symlink_metadata(path)?;
    if !current_metadata.is_file() || current_metadata.file_type().is_symlink() {
        return Err(anyhow!("config path changed while it was being edited"));
    }
    if !same_object(&current_metadata, &original.0) {
        return Err(anyhow!("config path changed while it was being edited"));
    }
    let current = super::read_regular_file(path, Some(&original.0))?;
    if current != original.1 {
        return Err(anyhow!("config changed while it was being edited"));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn restore_target(target: &mut fs::File, original: &[u8]) -> Result<()> {
    target.set_len(0)?;
    target.rewind()?;
    target.write_all(original)?;
    target.sync_all()?;
    target.rewind()?;
    let mut verified = Vec::new();
    target.read_to_end(&mut verified)?;
    if verified != original {
        return Err(anyhow!("restored config bytes do not match the original"));
    }
    Ok(())
}
