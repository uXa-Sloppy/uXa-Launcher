//! uXa Server Agent. Runs on the game machine, verifies signed requests,
//! and swaps in a Friday Night Fights dedicated-server build.

mod config;
mod http;
mod runtime;
mod schedule;
mod update;

use std::path::Path;

pub fn init_logging(log_dir: &Path) {
    use tracing_subscriber::prelude::*;
    let _ = std::fs::create_dir_all(log_dir);
    let appender = tracing_appender::rolling::daily(log_dir, "agent.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .with(tracing_subscriber::fmt::layer())
        .try_init();
    std::mem::forget(guard);
}

#[cfg(windows)]
mod service;

pub use config::{load_config, AgentConfig, AutoUpdateConfig};
pub use http::{serve, serve_listener};
pub use runtime::{Agent, Fail};
pub use update::SelfUpdateOutcome;

#[cfg(windows)]
pub use service::run_service;
