use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_and_help_are_available() {
    Command::cargo_bin("resync")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("protocol resync.v1"));
    for arguments in [
        vec!["--help"],
        vec!["auth", "--help"],
        vec!["auth", "login", "--help"],
        vec!["project", "--help"],
        vec!["project", "join", "--help"],
        vec!["device", "--help"],
        vec!["peer", "sync", "--help"],
        vec!["daemon", "--help"],
    ] {
        Command::cargo_bin("resync")
            .unwrap()
            .args(arguments)
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage:"));
    }
}
