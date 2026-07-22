mod common;

use common::{fixture, rev};
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
