fn main() {
    if let Err(error) = cdr_corel::run_cli() {
        eprintln!("cdr-corel: {error}");
        std::process::exit(1);
    }
}
