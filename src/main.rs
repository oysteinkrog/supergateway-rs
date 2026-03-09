mod cli;

fn main() {
    let config = cli::Config::parse();

    if config.log_level != cli::LogLevel::None {
        eprintln!(
            "supergateway: {} -> {} on port {}",
            config.input_value, config.output_transport, config.port
        );
    }

    // Gateway dispatch will be implemented by downstream beads.
    eprintln!("supergateway: gateway not yet implemented");
    std::process::exit(1);
}
