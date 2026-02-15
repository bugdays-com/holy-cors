mod config;
mod cors;
mod proxy;

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use crate::config::Config;
use crate::proxy::handle_request;

const BANNER: &str = r#"
    _   _       _          ____  ___  ____  ____  _
   | | | | ___ | |_   _   / ___|/ _ \|  _ \/ ___|| |
   | |_| |/ _ \| | | | | | |   | | | | |_) \___ \| |
   |  _  | (_) | | |_| | | |___| |_| |  _ < ___) |_|
   |_| |_|\___/|_|\__, |  \____|\___/|_| \_\____/(_)
                  |___/
"#;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Install the default TLS crypto provider (ring)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // Parse CLI arguments
    let config = Config::parse();
    let config = Arc::new(config);

    // Initialize logging
    let log_level = if config.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };

    FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();

    // Print banner
    println!("{}", BANNER);
    println!("  A fast CORS proxy for developers\n");

    // Print configuration
    info!("Starting Holy CORS proxy...");
    info!("Listening on http://{}", config.socket_addr());

    if config.allow_all {
        info!("Mode: Allow ALL origins (development mode)");
    } else {
        info!("Allowed origins:");
        for origin in config.allowed_origins() {
            info!("  - {}", origin);
        }
    }

    println!();
    info!("Usage: http://localhost:{}/{{TARGET_URL}}", config.port);
    info!("Example: http://localhost:{}/https://api.github.com/users/octocat", config.port);
    println!();

    // Bind to address
    let addr: SocketAddr = config.socket_addr().parse()?;
    let listener = TcpListener::bind(addr).await?;

    info!("Server is ready to accept connections");

    // Accept connections
    loop {
        let (stream, remote_addr) = match listener.accept().await {
            Ok(result) => result,
            Err(e) => {
                error!("Failed to accept connection: {}", e);
                continue;
            }
        };

        let config = Arc::clone(&config);

        // Spawn a new task for each connection
        tokio::spawn(async move {
            let io = TokioIo::new(stream);

            let service = service_fn(move |req| {
                let config = Arc::clone(&config);
                async move { handle_request(req, config).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                if !e.to_string().contains("connection closed") {
                    error!("Connection error from {}: {}", remote_addr, e);
                }
            }
        });
    }
}
