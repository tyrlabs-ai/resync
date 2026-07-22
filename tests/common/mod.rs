#![allow(dead_code)]

use resync::process::{RunOptions, git};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct Fixture {
    pub root: TempDir,
    pub remote: PathBuf,
    pub seed: PathBuf,
    pub local: PathBuf,
    pub base: String,
}

pub fn fixture() -> anyhow::Result<Fixture> {
    let root = tempfile::tempdir()?;
    let remote = root.path().join("remote.git");
    let seed = root.path().join("seed");
    let local = root.path().join("local");
    git(
        root.path(),
        [
            "init",
            "--bare",
            "--initial-branch=main",
            remote.to_string_lossy().as_ref(),
        ],
        RunOptions::default(),
    )?;
    git(
        root.path(),
        [
            "init",
            "--initial-branch=main",
            seed.to_string_lossy().as_ref(),
        ],
        RunOptions::default(),
    )?;
    configure(&seed)?;
    fs::write(seed.join("file"), "base\n")?;
    git(&seed, ["add", "."], RunOptions::default())?;
    git(&seed, ["commit", "-m", "base"], RunOptions::default())?;
    git(
        &seed,
        ["remote", "add", "origin", remote.to_string_lossy().as_ref()],
        RunOptions::default(),
    )?;
    git(&seed, ["push", "origin", "main"], RunOptions::default())?;
    git(
        root.path(),
        [
            "clone",
            remote.to_string_lossy().as_ref(),
            local.to_string_lossy().as_ref(),
        ],
        RunOptions::default(),
    )?;
    configure(&local)?;
    let base = rev(&local, "HEAD")?;
    Ok(Fixture {
        root,
        remote,
        seed,
        local,
        base,
    })
}

pub fn configure(path: &Path) -> anyhow::Result<()> {
    git(path, ["config", "user.name", "Test"], RunOptions::default())?;
    git(
        path,
        ["config", "user.email", "test@example.invalid"],
        RunOptions::default(),
    )?;
    Ok(())
}

pub fn rev(path: &Path, value: &str) -> anyhow::Result<String> {
    Ok(git(path, ["rev-parse", value], RunOptions::default())?
        .stdout
        .trim()
        .into())
}

pub fn advance_remote(fixture: &Fixture, contents: &str) -> anyhow::Result<String> {
    fs::write(fixture.seed.join("file"), contents)?;
    git(
        &fixture.seed,
        ["commit", "-am", "remote advance"],
        RunOptions::default(),
    )?;
    git(
        &fixture.seed,
        ["push", "origin", "main"],
        RunOptions::default(),
    )?;
    let target = rev(&fixture.seed, "HEAD")?;
    git(&fixture.local, ["fetch", "origin"], RunOptions::default())?;
    Ok(target)
}
