fn foreground() -> Result<(), String> {
    let config = uxa_server_agent::load_config()?;
    uxa_server_agent::init_logging(&config.log_dir);
    tracing::info!("uXa Server Agent {}", env!("CARGO_PKG_VERSION"));
    let agent = uxa_server_agent::Agent::new(config)?;
    let runtime = tokio::runtime::Runtime::new().map_err(|err| err.to_string())?;
    runtime.block_on(uxa_server_agent::serve(agent))
}

fn main() {
    // axum-server pulls in aws-lc-rs and reqwest pulls in ring, so rustls cannot pick a provider by itself.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let service = std::env::args().any(|arg| arg == "--service");
    if service {
        #[cfg(windows)]
        {
            if let Err(err) = uxa_server_agent::run_service() {
                eprintln!("uXa Server Agent service failed: {err}");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(windows))]
        {
            eprintln!("--service is only supported on Windows. Run uxa-server-agent in the foreground.");
            std::process::exit(1);
        }
    }
    if let Err(err) = foreground() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
