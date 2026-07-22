use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Default, Clone)]
pub struct RunOptions {
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub input: Option<Vec<u8>>,
    pub allow_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn run<P, I, S>(program: P, args: I, options: RunOptions) -> Result<RunResult>
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref();
    let arguments: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    let mut command = Command::new(program);
    command
        .args(&arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &options.cwd {
        command.current_dir(cwd);
    }
    command.envs(&options.env);
    if options.input.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {}", program.to_string_lossy()))?;
    if let Some(input) = options.input {
        child
            .stdin
            .take()
            .context("child stdin unavailable")?
            .write_all(&input)?;
    }
    let output = child.wait_with_output()?;
    let result = RunResult {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    if result.code != 0 && !options.allow_failure {
        let detail = if !result.stderr.trim().is_empty() {
            result.stderr.trim()
        } else if !result.stdout.trim().is_empty() {
            result.stdout.trim()
        } else {
            "process exited unsuccessfully"
        };
        bail!(
            "{} {} failed: {}",
            program.to_string_lossy(),
            arguments
                .iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            detail
        );
    }
    Ok(result)
}

pub fn git<I, S>(cwd: &Path, args: I, mut options: RunOptions) -> Result<RunResult>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    options.cwd = Some(cwd.to_owned());
    run("git", args, options)
}
