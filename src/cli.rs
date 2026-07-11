use std::io::Write;
use std::path::Path;
use std::time::Duration;

use reqwest::Client;
use serde_json::{Value, json};

use crate::config::EffectiveConfiguration;
use crate::error::{AppError, CliExit, Retryability};
use crate::safe_file::read_regular_utf8;

pub async fn get(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    path: &str,
    json_output: bool,
) -> Result<(), AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .get(endpoint(&server, path))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| AppError::unavailable())?;
    emit_response(response, json_output).await
}

pub async fn validate(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    path: &Path,
    json_output: bool,
) -> Result<(), AppError> {
    if path.as_os_str() == "-" {
        return Err(AppError::usage(
            "configuration validation requires FILE; stdin is prohibited",
        ));
    }
    let content = read_regular_utf8(path, "configuration proposal")?;
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .post(endpoint(&server, "/api/v1/system/configuration/validate"))
        .header("accept", "application/json")
        .json(&json!({ "content": content }))
        .send()
        .await
        .map_err(|_| AppError::unavailable())?;
    emit_response(response, json_output).await
}

fn client(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
) -> Result<(Client, String), AppError> {
    let configuration = EffectiveConfiguration::client(server_override)?;
    let timeout_ms = timeout_override_ms.unwrap_or(configuration.client.timeout_ms);
    if !(1..=3_600_000).contains(&timeout_ms) {
        return Err(AppError::usage("--timeout must be between 1ms and 1h"));
    }
    let client = Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .no_proxy()
        .build()
        .map_err(|error| AppError::internal(format!("cannot create HTTP client: {error}")))?;
    Ok((client, configuration.client.server))
}

async fn emit_response(response: reqwest::Response, json_output: bool) -> Result<(), AppError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| AppError::unavailable())?;
    let envelope = serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| AppError::internal(format!("daemon returned invalid JSON: {error}")))?;
    if status.is_success() {
        let rendered = if json_output {
            serde_json::to_vec(&envelope)
        } else {
            let result = envelope.get("result").cloned().unwrap_or(Value::Null);
            serde_json::to_vec_pretty(&result).map(|mut rendered| {
                let mut human = b"Observatory result:\n".to_vec();
                human.append(&mut rendered);
                human
            })
        }
        .map_err(|error| AppError::internal(format!("cannot render daemon result: {error}")))?;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&rendered)
            .and_then(|()| stdout.write_all(b"\n"))
            .map_err(|error| AppError::internal(format!("cannot write stdout: {error}")))?;
        return Ok(());
    }
    let error = envelope.get("error").ok_or_else(|| {
        AppError::internal(format!("daemon returned HTTP {status} without an error"))
    })?;
    Err(AppError::remote(
        error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("daemon_request_failed")
            .to_owned(),
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the daemon request failed")
            .to_owned(),
        Retryability::from_bool(
            error
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| status.is_server_error()),
        ),
        error.get("details").cloned().unwrap_or_else(|| json!({})),
        http_exit(status.as_u16()),
    ))
}

fn endpoint(server: &str, path: &str) -> String {
    format!("{}{path}", server.trim_end_matches('/'))
}

const fn http_exit(status: u16) -> CliExit {
    match status {
        404 | 410 => CliExit::NotFound,
        409 | 412 | 428 => CliExit::Conflict,
        423 | 429 => CliExit::Contention,
        500 | 507 => CliExit::Internal,
        503 => CliExit::Unavailable,
        _ => CliExit::Usage,
    }
}
