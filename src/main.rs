#![warn(
    clippy::pedantic,
    clippy::cargo,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::dbg_macro,
    clippy::print_stdout,
    clippy::exit,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::missing_safety_doc
)]
#![allow(clippy::module_name_repetitions)]
#![deny(clippy::all, clippy::correctness, clippy::suspicious)]

mod catalogue;
mod cli;
mod config;
mod daemon;
mod error;
mod runtime_lock;
mod safe_file;
mod storage_status;
mod web;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use config::ServeOverrides;
use error::AppError;

#[derive(Debug, Parser)]
#[command(
    name = "obs",
    version,
    about = "Observatory catalogue client and daemon"
)]
struct CommandLine {
    #[arg(long, global = true)]
    server: Option<String>,
    #[arg(short = 'p', long, global = true)]
    project: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true, value_parser = parse_duration)]
    timeout: Option<u64>,
    #[arg(long, global = true)]
    idempotency_key: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve(ServeCommand),
    System(SystemCommand),
}

#[derive(Debug, Args)]
struct ServeCommand {
    #[arg(long)]
    listen: Option<String>,
    #[arg(long)]
    canonical_origin: Option<String>,
    #[arg(long)]
    storage: Option<PathBuf>,
    #[arg(long)]
    max_stored_bytes: Option<u64>,
    #[arg(long)]
    max_live_artifacts: Option<u64>,
    #[arg(long, value_parser = parse_duration)]
    teardown_timeout: Option<u64>,
}

#[derive(Debug, Args)]
struct SystemCommand {
    #[command(subcommand)]
    command: SystemLeaf,
}

#[derive(Debug, Subcommand)]
enum SystemLeaf {
    Status,
    Config(ConfigCommand),
}

#[derive(Debug, Args)]
struct ConfigCommand {
    #[command(subcommand)]
    command: ConfigLeaf,
}

#[derive(Debug, Subcommand)]
enum ConfigLeaf {
    Show,
    Validate { file: PathBuf },
}

fn main() -> ExitCode {
    let command = CommandLine::parse();
    let json_output = command.json;
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .max_blocking_threads(4)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return report(
                &AppError::internal(format!("cannot start runtime: {error}")),
                json_output,
            );
        }
    };
    match runtime.block_on(run(command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report(&error, json_output),
    }
}

async fn run(command: CommandLine) -> Result<(), AppError> {
    let CommandLine {
        server,
        project: _,
        json,
        timeout,
        idempotency_key: _,
        command,
    } = command;
    match command {
        Command::Serve(serve) => {
            if server.is_some() || timeout.is_some() {
                return Err(AppError::usage(
                    "--server and --timeout do not configure obs serve",
                ));
            }
            daemon::serve(ServeOverrides {
                listen: serve.listen,
                canonical_origin: serve.canonical_origin,
                storage: serve.storage,
                max_stored_bytes: serve.max_stored_bytes,
                max_live_artifacts: serve.max_live_artifacts,
                teardown_timeout_ms: serve.teardown_timeout,
            })
            .await
        }
        Command::System(system) => match system.command {
            SystemLeaf::Status => cli::get(server, timeout, "/api/v1/system/status", json).await,
            SystemLeaf::Config(config) => match config.command {
                ConfigLeaf::Show => {
                    cli::get(server, timeout, "/api/v1/system/configuration", json).await
                }
                ConfigLeaf::Validate { file } => cli::validate(server, timeout, &file, json).await,
            },
        },
    }
}

fn report(error: &AppError, json_output: bool) -> ExitCode {
    let mut stderr = std::io::stderr().lock();
    if json_output {
        let _ = serde_json::to_writer(&mut stderr, &error.envelope());
        let _ = stderr.write_all(b"\n");
    } else {
        let _ = writeln!(stderr, "error: {} ({})", error.message, error.code());
    }
    ExitCode::from(error.exit_code())
}

fn parse_duration(value: &str) -> Result<u64, String> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        (value, 1)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier))
        .ok_or_else(|| "duration must be an unsigned integer with ms, s, m, or h suffix".into())
}
