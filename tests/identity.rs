mod common;

use common::{configure, fixture};
use resync::identity::{read_identity, same_project, write_identity};
use resync::process::{RunOptions, git};

#[test]
fn clones_do_not_inherit_checkout_identity() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let identity = write_identity(&fixture.local, "https://provider.example", "prj_one", None)?;
    let clone = fixture.root.path().join("clone");
    git(
        fixture.root.path(),
        [
            "clone",
            fixture.local.to_string_lossy().as_ref(),
            clone.to_string_lossy().as_ref(),
        ],
        RunOptions::default(),
    )?;
    configure(&clone)?;
    let cloned = read_identity(&clone)?;
    assert!(cloned.project_id.is_none());
    assert!(cloned.checkout_id.is_none());
    let second = write_identity(&clone, "https://provider.example", "prj_one", None)?;
    assert!(same_project(&identity, &second)?);
    assert_ne!(identity.checkout_id, second.checkout_id);
    Ok(())
}

#[test]
fn identical_history_can_have_independent_project_ids() -> anyhow::Result<()> {
    let fixture = fixture()?;
    let first = write_identity(&fixture.local, "https://provider.example", "prj_one", None)?;
    let second_path = fixture.root.path().join("second");
    git(
        fixture.root.path(),
        [
            "clone",
            fixture.local.to_string_lossy().as_ref(),
            second_path.to_string_lossy().as_ref(),
        ],
        RunOptions::default(),
    )?;
    let second = write_identity(&second_path, "https://provider.example", "prj_two", None)?;
    assert!(!same_project(&first, &second)?);
    Ok(())
}
