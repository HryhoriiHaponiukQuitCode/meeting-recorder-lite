//! Daily-rotated log file (7 days kept) plus optional console output.
//! `RECORDER_LOG=debug` raises the level.

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;

/// Keep the guard alive for the whole program, or buffered lines are lost.
pub fn init(console: Option<tracing::Level>) -> Result<WorkerGuard> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, filter::LevelFilter, fmt};
    let dir = crate::paths::log_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create log dir {}", dir.display()))?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("recorder")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("RECORDER_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let console = console.map(|level| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .with_filter(LevelFilter::from_level(level))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .with(console)
        .init();
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(%info, "panic");
        eprintln!("{info}");
    }));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), log_dir = %dir.display(), os = std::env::consts::OS, "started");
    Ok(guard)
}
