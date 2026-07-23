mod common;

use common::{advance_remote, fixture, rev};
use resync::process::{RunOptions, git};
use resync::protocol::{AdvertisedHead, CatalogProject};
use resync::remote::{fetch_remote, materialize_project};
use serde_json::Map;
use std::fs;

#[test]
fn materialization_and_fetch_track_every_hosted_branch() -> anyhow::Result<()> {
    let fixture = fixture()?;
    git(
        &fixture.seed,
        ["switch", "-c", "feature"],
        RunOptions::default(),
    )?;
    fs::write(fixture.seed.join("feature"), "feature\n")?;
    git(&fixture.seed, ["add", "."], RunOptions::default())?;
    git(
        &fixture.seed,
        ["commit", "-m", "feature"],
        RunOptions::default(),
    )?;
    git(
        &fixture.seed,
        ["push", "origin", "feature"],
        RunOptions::default(),
    )?;
    let feature = rev(&fixture.seed, "HEAD")?;
    let main = rev(&fixture.seed, "main")?;
    let destination = fixture.root.path().join("materialized");
    let project = CatalogProject {
        project_id: "prj_test".into(),
        name: "test".into(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        default_branch: "main".into(),
        advertised_heads: vec![
            AdvertisedHead {
                name: "main".into(),
                oid: main,
            },
            AdvertisedHead {
                name: "feature".into(),
                oid: feature.clone(),
            },
        ],
        project_generation: 1,
        extra: Map::new(),
    };
    let local = materialize_project(&project, &destination, None, "resync")?;
    assert_eq!(local.advertised_heads.get("feature"), Some(&feature));
    assert_eq!(fetch_remote(&local, None)?.len(), 2);
    Ok(())
}

#[test]
fn materialization_adopts_an_exact_repository_snapshot() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let destination = fixture.root.path().join("snapshot");
    fs::create_dir(&destination)?;
    fs::write(destination.join("file"), "base\n")?;
    fs::write(destination.join("local-note"), "preserve me\n")?;
    let remote_head = advance_remote(&fixture, "remote update\n")?;
    let project = CatalogProject {
        project_id: "prj_snapshot".into(),
        name: "snapshot".into(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        default_branch: "main".into(),
        advertised_heads: vec![AdvertisedHead {
            name: "main".into(),
            oid: remote_head.clone(),
        }],
        project_generation: 2,
        extra: Map::new(),
    };

    materialize_project(&project, &destination, None, "resync")?;

    assert_eq!(rev(&destination, "HEAD")?, remote_head);
    assert_eq!(
        fs::read_to_string(destination.join("file"))?,
        "remote update\n"
    );
    assert_eq!(
        fs::read_to_string(destination.join("local-note"))?,
        "preserve me\n"
    );
    Ok(())
}

#[test]
fn materialization_refuses_a_modified_non_git_directory() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let destination = fixture.root.path().join("modified-snapshot");
    fs::create_dir(&destination)?;
    fs::write(destination.join("file"), "locally modified\n")?;
    let project = CatalogProject {
        project_id: "prj_modified_snapshot".into(),
        name: "modified-snapshot".into(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        default_branch: "main".into(),
        advertised_heads: vec![AdvertisedHead {
            name: "main".into(),
            oid: fixture.base.clone(),
        }],
        project_generation: 1,
        extra: Map::new(),
    };

    let error = materialize_project(&project, &destination, None, "resync").unwrap_err();

    assert!(error.to_string().contains("refusing to adopt"));
    assert!(!destination.join(".git").exists());
    assert_eq!(
        fs::read_to_string(destination.join("file"))?,
        "locally modified\n"
    );
    Ok(())
}

#[test]
fn materialization_does_not_mistake_an_old_partial_tree_for_a_snapshot() -> anyhow::Result<()> {
    let fixture = fixture()?;
    fs::write(fixture.seed.join("added-later"), "hosted\n")?;
    git(&fixture.seed, ["add", "."], RunOptions::default())?;
    git(
        &fixture.seed,
        ["commit", "-m", "add another tracked file"],
        RunOptions::default(),
    )?;
    git(
        &fixture.seed,
        ["push", "origin", "main"],
        RunOptions::default(),
    )?;
    let remote_head = rev(&fixture.seed, "HEAD")?;
    let destination = fixture.root.path().join("partial-tree");
    fs::create_dir(&destination)?;
    fs::write(destination.join("file"), "base\n")?;
    fs::write(destination.join("added-later"), "different\n")?;
    let project = CatalogProject {
        project_id: "prj_partial_tree".into(),
        name: "partial-tree".into(),
        remote_url: fixture.remote.to_string_lossy().into_owned(),
        default_branch: "main".into(),
        advertised_heads: vec![AdvertisedHead {
            name: "main".into(),
            oid: remote_head,
        }],
        project_generation: 2,
        extra: Map::new(),
    };

    let error = materialize_project(&project, &destination, None, "resync").unwrap_err();

    assert!(error.to_string().contains("refusing to adopt"));
    assert!(!destination.join(".git").exists());
    assert_eq!(
        fs::read_to_string(destination.join("added-later"))?,
        "different\n"
    );
    Ok(())
}
