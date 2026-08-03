fn main() {
    if let Err(error) = resync::cli::run() {
        if let Some(structured) = resync::cli::structured_error(&error) {
            serde_json::to_writer_pretty(std::io::stdout(), &structured.value).unwrap();
            println!();
            eprintln!("resync: {}", structured.diagnostic);
            std::process::exit(structured.exit_code);
        }
        eprintln!("resync: {error:#}");
        std::process::exit(1);
    }
}
