use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn ensure_project_dispatcher(
    state_root: &Path,
    project_id: &str,
    executable: &Path,
) -> Result<PathBuf> {
    ensure_dispatcher(
        state_root,
        "codex",
        project_id.as_bytes(),
        "RepoSync Codex hook dispatcher v1",
        &format!("codex-hook {}", shell_quote(project_id)),
        executable,
    )
}

pub(crate) fn ensure_workspace_dispatcher(
    state_root: &Path,
    manifest_path: &Path,
    executable: &Path,
) -> Result<PathBuf> {
    ensure_dispatcher(
        state_root,
        "codex-workspace",
        manifest_path.as_os_str().as_encoded_bytes(),
        "RepoSync Codex workspace hook dispatcher v1",
        &format!(
            "codex-workspace-hook {}",
            shell_quote(&manifest_path.to_string_lossy())
        ),
        executable,
    )
}

fn ensure_dispatcher(
    state_root: &Path,
    prefix: &str,
    identity: &[u8],
    description: &str,
    arguments: &str,
    executable: &Path,
) -> Result<PathBuf> {
    let digest = format!("{:x}", Sha256::digest(identity));
    let directory = state_root.join("hook-dispatchers");
    let dispatcher = directory.join(format!("{prefix}-{digest}"));
    fs::create_dir_all(&directory)
        .with_context(|| format!("create hook dispatcher directory {}", directory.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;

    let script = format!("#!/bin/sh\n# {description}\nexec \"$0.resync\" {arguments}\n");
    if fs::read_to_string(&dispatcher).ok().as_deref() != Some(&script) {
        fs::write(&dispatcher, script)
            .with_context(|| format!("write hook dispatcher {}", dispatcher.display()))?;
    }
    #[cfg(unix)]
    fs::set_permissions(&dispatcher, fs::Permissions::from_mode(0o700))?;

    let target = dispatcher.with_extension("resync");
    let temporary = target.with_extension(format!("resync.tmp.{}", std::process::id()));
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    symlink(executable, &temporary)
        .with_context(|| format!("link hook dispatcher target {}", temporary.display()))?;
    #[cfg(not(unix))]
    fs::copy(executable, &temporary)
        .with_context(|| format!("copy hook dispatcher target {}", temporary.display()))?;
    fs::rename(&temporary, &target)
        .with_context(|| format!("activate hook dispatcher target {}", target.display()))?;
    Ok(dispatcher)
}

pub(crate) fn is_legacy_project_command(command: &str, project_id: &str) -> bool {
    let suffix = format!(" codex-hook {}", shell_quote(project_id));
    let Some(executable) = command.strip_suffix(&suffix) else {
        return false;
    };
    Path::new(executable.trim_matches(['\'', '"']))
        .file_name()
        .and_then(|value| value.to_str())
        == Some("resync")
}

pub(crate) fn is_legacy_workspace_command(command: &str, manifest_path: &Path) -> bool {
    let suffix = format!(
        " codex-workspace-hook {}",
        shell_quote(&manifest_path.to_string_lossy())
    );
    let Some(executable) = command.strip_suffix(&suffix) else {
        return false;
    };
    Path::new(executable.trim_matches(['\'', '"']))
        .file_name()
        .and_then(|value| value.to_str())
        == Some("resync")
}
