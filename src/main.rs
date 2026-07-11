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
mod crypto;
mod csrf;
mod daemon;
mod error;
mod project;
mod runtime_lock;
mod safe_file;
mod storage_status;
mod ui;
mod web;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use config::ServeOverrides;
use error::AppError;
use project::{RegisterProjectRequest, TombstoneProjectRequest, UpdateProjectRequest};

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
    Project(ProjectCommand),
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

#[derive(Debug, Args)]
struct ProjectCommand {
    #[command(subcommand)]
    command: ProjectLeaf,
}

#[derive(Debug, Subcommand)]
enum ProjectLeaf {
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        query: Option<String>,
        #[arg(long, value_enum)]
        order: Option<ProjectListOrder>,
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=200))]
        limit: Option<u16>,
        #[arg(long)]
        after: Option<String>,
    },
    Show {
        selector: String,
    },
    Resolve {
        path: Option<PathBuf>,
    },
    Register {
        path: Option<PathBuf>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        slug: Option<String>,
    },
    Update {
        selector: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        slug: Option<String>,
    },
    Tombstone {
        selector: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProjectListOrder {
    Recent,
    Title,
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

#[derive(Clone)]
struct ClientContext {
    server: Option<String>,
    timeout: Option<u64>,
    json: bool,
}

struct ProjectClientContext {
    client: ClientContext,
    selected_project: Option<PathBuf>,
    idempotency_key: Option<String>,
}

async fn run(command: CommandLine) -> Result<(), AppError> {
    let client = ClientContext {
        server: command.server,
        timeout: command.timeout,
        json: command.json,
    };
    match command.command {
        Command::Serve(serve) => run_serve(serve, client).await,
        Command::System(system) => run_system(system, client).await,
        Command::Project(project) => {
            run_project(
                project,
                ProjectClientContext {
                    client,
                    selected_project: command.project,
                    idempotency_key: command.idempotency_key,
                },
            )
            .await
        }
    }
}

async fn run_serve(command: ServeCommand, client: ClientContext) -> Result<(), AppError> {
    if client.server.is_some() || client.timeout.is_some() {
        return Err(AppError::usage(
            "--server and --timeout do not configure obs serve",
        ));
    }
    daemon::serve(ServeOverrides {
        listen: command.listen,
        canonical_origin: command.canonical_origin,
        storage: command.storage,
        max_stored_bytes: command.max_stored_bytes,
        max_live_artifacts: command.max_live_artifacts,
        teardown_timeout_ms: command.teardown_timeout,
    })
    .await
}

async fn run_system(command: SystemCommand, client: ClientContext) -> Result<(), AppError> {
    match command.command {
        SystemLeaf::Status => {
            cli::get(
                client.server,
                client.timeout,
                "/api/v1/system/status",
                client.json,
            )
            .await
        }
        SystemLeaf::Config(config) => run_config(config, client).await,
    }
}

async fn run_config(command: ConfigCommand, client: ClientContext) -> Result<(), AppError> {
    match command.command {
        ConfigLeaf::Show => {
            cli::get(
                client.server,
                client.timeout,
                "/api/v1/system/configuration",
                client.json,
            )
            .await
        }
        ConfigLeaf::Validate { file } => {
            cli::validate(client.server, client.timeout, &file, client.json).await
        }
    }
}

async fn run_project(
    command: ProjectCommand,
    context: ProjectClientContext,
) -> Result<(), AppError> {
    match command.command {
        ProjectLeaf::List {
            all,
            query,
            order,
            limit,
            after,
        } => run_project_list(context.client, all, query, order, limit, after).await,
        ProjectLeaf::Resolve { path } => run_project_resolve(context, path).await,
        ProjectLeaf::Register { path, title, slug } => {
            run_project_register(context, path, title, slug).await
        }
        ProjectLeaf::Show { selector } => run_project_show(context.client, &selector).await,
        ProjectLeaf::Update {
            selector,
            title,
            slug,
        } => run_project_update(context, &selector, title, slug).await,
        ProjectLeaf::Tombstone { selector, yes } => {
            run_project_tombstone(context, &selector, yes).await
        }
    }
}

async fn run_project_list(
    client: ClientContext,
    all: bool,
    query: Option<String>,
    order: Option<ProjectListOrder>,
    limit: Option<u16>,
    after: Option<String>,
) -> Result<(), AppError> {
    let parameters = project_list_parameters(all, query, order, limit, after);
    cli::get_with_query(
        client.server,
        client.timeout,
        "/api/v1/projects",
        &parameters,
        client.json,
    )
    .await
}

fn project_list_parameters(
    all: bool,
    query: Option<String>,
    order: Option<ProjectListOrder>,
    limit: Option<u16>,
    after: Option<String>,
) -> Vec<(String, String)> {
    let mut parameters = Vec::new();
    if all {
        parameters.push(("state".to_owned(), "all".to_owned()));
    }
    if let Some(query) = query {
        parameters.push(("query".to_owned(), query));
    }
    if let Some(order) = order {
        parameters.push(("order".to_owned(), order.as_str().to_owned()));
    }
    if let Some(limit) = limit {
        parameters.push(("limit".to_owned(), limit.to_string()));
    }
    if let Some(after) = after {
        parameters.push(("after".to_owned(), after));
    }
    parameters
}

async fn run_project_resolve(
    context: ProjectClientContext,
    path: Option<PathBuf>,
) -> Result<(), AppError> {
    let path = absolute_project_selection(path.or(context.selected_project))?;
    cli::get_with_query(
        context.client.server,
        context.client.timeout,
        "/api/v1/projects/resolve",
        &[("path".to_owned(), path)],
        context.client.json,
    )
    .await
}

async fn run_project_register(
    context: ProjectClientContext,
    path: Option<PathBuf>,
    title: Option<String>,
    slug: Option<String>,
) -> Result<(), AppError> {
    let path = absolute_project_selection(path.or(context.selected_project))?;
    let key = mutation_idempotency_key(context.idempotency_key)?;
    cli::post(
        context.client.server,
        context.client.timeout,
        "/api/v1/projects",
        &key,
        &RegisterProjectRequest { path, title, slug },
        context.client.json,
    )
    .await
}

struct CurrentProject {
    key: String,
    etag: String,
    api_path: String,
}

async fn run_project_update(
    context: ProjectClientContext,
    selector: &str,
    title: Option<String>,
    slug: Option<String>,
) -> Result<(), AppError> {
    let idempotency_key = mutation_idempotency_key(context.idempotency_key)?;
    let project = current_project(&context.client, selector).await?;
    cli::patch(
        context.client.server,
        context.client.timeout,
        cli::ExistingResourceMutation::new(
            &project.api_path,
            &project.etag,
            &idempotency_key,
            &UpdateProjectRequest::new(title, slug),
        ),
        context.client.json,
    )
    .await
}

async fn run_project_tombstone(
    context: ProjectClientContext,
    selector: &str,
    confirmed: bool,
) -> Result<(), AppError> {
    if !confirmed {
        return Err(AppError::invalid(
            "confirmation_required",
            "Project tombstone requires --yes",
        ));
    }
    let idempotency_key = mutation_idempotency_key(context.idempotency_key)?;
    let project = current_project(&context.client, selector).await?;
    cli::delete(
        context.client.server,
        context.client.timeout,
        cli::ExistingResourceMutation::new(
            &project.api_path,
            &project.etag,
            &idempotency_key,
            &TombstoneProjectRequest::new(project.key),
        ),
        context.client.json,
    )
    .await
}

async fn current_project(
    client: &ClientContext,
    selector: &str,
) -> Result<CurrentProject, AppError> {
    let id = resolve_project_selector(selector, client.server.clone(), client.timeout).await?;
    let resource = cli::fetch_resource(
        client.server.clone(),
        client.timeout,
        &format!("/api/v1/projects/{id}"),
    )
    .await?;
    let project = resource
        .envelope()
        .get("result")
        .ok_or_else(|| AppError::internal("Project response has no result"))?;
    let key = project
        .get("key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::internal("Project response has no key"))?
        .to_owned();
    let api_url = project
        .get("apiUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::internal("Project response has no API URL"))?;
    let api_url = url::Url::parse(api_url)
        .map_err(|error| AppError::internal(format!("Project API URL is invalid: {error}")))?;
    if api_url.query().is_some() || api_url.fragment().is_some() {
        return Err(AppError::internal("Project API URL is not canonical"));
    }
    Ok(CurrentProject {
        key,
        etag: resource.etag().to_owned(),
        api_path: api_url.path().to_owned(),
    })
}

async fn run_project_show(client: ClientContext, selector: &str) -> Result<(), AppError> {
    let id = resolve_project_selector(selector, client.server.clone(), client.timeout).await?;
    cli::get(
        client.server,
        client.timeout,
        &format!("/api/v1/projects/{id}"),
        client.json,
    )
    .await
}

impl ProjectListOrder {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Title => "title",
        }
    }
}

fn absolute_project_selection(selection: Option<PathBuf>) -> Result<String, AppError> {
    let path = match selection {
        Some(path) if path.is_absolute() => path,
        Some(path) => std::env::current_dir()
            .map_err(|error| AppError::usage(format!("cannot read current directory: {error}")))?
            .join(path),
        None => std::env::current_dir()
            .map_err(|error| AppError::usage(format!("cannot read current directory: {error}")))?,
    };
    path.into_os_string()
        .into_string()
        .map_err(|_| AppError::usage("Project path must be valid UTF-8"))
}

fn mutation_idempotency_key(key: Option<String>) -> Result<String, AppError> {
    if let Some(key) = key {
        return Ok(key);
    }
    if !std::io::stderr().is_terminal() {
        return Err(AppError::invalid(
            "invalid_idempotency_key",
            "non-TTY mutations require --idempotency-key",
        ));
    }
    let key = format!("interactive-{}", crypto::random_opaque_id()?);
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "Idempotency-Key: {key}")
        .map_err(|error| AppError::internal(format!("cannot write stderr: {error}")))?;
    Ok(key)
}

async fn resolve_project_selector(
    selector: &str,
    server: Option<String>,
    timeout: Option<u64>,
) -> Result<String, AppError> {
    let candidate = selector.rsplit_once('~').map_or(selector, |(_, id)| id);
    if is_project_id(candidate) {
        return Ok(candidate.to_owned());
    }
    let path = absolute_project_selection(Some(PathBuf::from(selector)))?;
    let envelope = cli::fetch_with_query(
        server,
        timeout,
        "/api/v1/projects/resolve",
        &[("path".to_owned(), path)],
    )
    .await?;
    let result = envelope
        .get("result")
        .ok_or_else(|| AppError::internal("Project resolve result is missing"))?;
    result
        .get("project")
        .and_then(|project| project.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            AppError::not_found_code(
                "project_not_registered",
                "Project selector does not resolve to a live Project",
            )
        })
}

fn is_project_id(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| b"0123456789abcdefghjkmnpqrstvwxyz".contains(&byte))
        && value.as_bytes()[0] <= b'7'
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
