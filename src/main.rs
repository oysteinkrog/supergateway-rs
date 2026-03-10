mod cli;
mod health;
mod observe;
mod session;

fn main() {
    let config = cli::Config::parse();

    let logger = observe::Logger::new(config.output_transport, config.log_level);
    let _metrics = observe::Metrics::new();

    logger.startup(
        env!("CARGO_PKG_VERSION"),
        &config.input_value,
        &config.output_transport.to_string(),
        config.port,
    );

    // Gateway dispatch will be implemented by downstream beads.
    logger.error("gateway not yet implemented");
    std::process::exit(1);
}
