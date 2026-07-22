fn main() {
    if let Err(error) = resync::cli::run() {
        eprintln!("resync: {error:#}");
        std::process::exit(1);
    }
}
