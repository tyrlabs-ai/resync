use anyhow::Result;

pub fn run() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|value| value == "--version") {
        println!(
            "resync {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            crate::protocol::PROTOCOL_VERSION
        );
        return Ok(());
    }
    println!(
        "RepoSync {} Rust migration build",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}
