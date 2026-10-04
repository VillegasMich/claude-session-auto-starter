//! claude-session-starter: keeps a Claude subscription 5-hour usage window running.
//!
//! Behavior is specified in `docs/architecture.md`; configuration in `docs/configuration.md`.

mod app;
mod claude;
mod clock;
mod config;
mod exec;
mod hours;
mod preflight;
mod probe;
mod retry;
mod scheduler;
mod starter;
mod state;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use signal_hook::consts::{SIGINT, SIGTERM};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

use crate::clock::SystemClock;
use crate::config::Config;

/// Keep a Claude Pro/Max 5-hour usage window running by sending one minimal message through
/// Claude Code whenever no window is active.
///
/// All configuration comes from environment variables (CLAUDE_CODE_OAUTH_TOKEN, ACTIVE_HOURS,
/// ...); see docs/configuration.md.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Copy, Subcommand)]
enum Command {
    /// Run forever: check, start a window when none is active, sleep until it resets. Default.
    Daemon,
    /// Check now, start a window if none is active (inside ACTIVE_HOURS), then exit.
    Once,
    /// Read-only: is a window active, when does it reset, last start, next check.
    Status,
    /// Validate configuration, the `claude` binary, authentication and detection.
    Check,
    /// Send the starter message (ignores ACTIVE_HOURS).
    Start {
        /// Send even if a window is already active.
        #[arg(long)]
        force: bool,
        /// Print the exact command and environment (token redacted) without running it.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> ExitCode {
    let command = Cli::parse().command.unwrap_or(Command::Daemon);
    // Before logging, so RUST_LOG from the file applies; and while still single-threaded.
    let dotenv = load_dotenv();
    init_logging(command);
    match dotenv {
        Ok(true) => debug!("loaded ./.env (development build)"),
        Ok(false) => {}
        Err(e) => {
            error!("cannot load ./.env: {e}");
            return ExitCode::FAILURE;
        }
    }
    let shutdown = match install_signal_handlers() {
        Ok(flag) => flag,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::FAILURE;
        }
    };
    match run(command, Arc::clone(&shutdown)) {
        Ok(()) => ExitCode::SUCCESS,
        // Stopped (SIGTERM) while a command was still running: not a failure.
        Err(e) if shutdown.load(Ordering::Relaxed) => {
            info!(reason = format!("{e:#}"), "stopped");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, shutdown: Arc<AtomicBool>) -> Result<()> {
    let config = Config::from_env()?;
    let clock = SystemClock::new(shutdown);
    match command {
        Command::Daemon => app::daemon(config, clock),
        Command::Once => app::once(config, clock),
        Command::Status => app::status(config, clock),
        Command::Check => app::check(config, clock),
        Command::Start { force, dry_run } => app::start(config, clock, force, dry_run),
    }
}

/// Development convenience: debug builds (`cargo run`) load `./.env` if it exists. Variables
/// already set in the environment win. Release builds (Docker, systemd) never read it: there the
/// environment comes from `--env-file` / `EnvironmentFile=`. Returns whether a file was loaded.
fn load_dotenv() -> Result<bool, dotenvy::Error> {
    if !cfg!(debug_assertions) {
        return Ok(false);
    }
    match dotenvy::from_path(".env") {
        Ok(()) => Ok(true),
        Err(e) if e.not_found() => Ok(false),
        Err(e) => Err(e),
    }
}

/// First SIGTERM/SIGINT requests a graceful shutdown; a second one exits immediately.
fn install_signal_handlers() -> Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register_conditional_shutdown(signal, 1, Arc::clone(&shutdown))
            .context("installing signal handler")?;
        signal_hook::flag::register(signal, Arc::clone(&shutdown))
            .context("installing signal handler")?;
    }
    Ok(shutdown)
}

fn init_logging(command: Command) {
    // Report commands print their own output; keep logs quiet unless RUST_LOG says otherwise.
    let default_level = match command {
        Command::Daemon | Command::Once | Command::Start { .. } => "info",
        Command::Status | Command::Check => "warn",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stdout)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}
