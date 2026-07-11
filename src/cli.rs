use std::io::Write;
use std::path::Path;
use std::time::Duration;

use reqwest::Client;
use serde::Serialize;
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
        .map_err(|error| request_error(&error, None))?;
    emit_response(response, json_output, None).await
}

pub async fn get_with_query(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    path: &str,
    query: &[(String, String)],
    json_output: bool,
) -> Result<(), AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .get(endpoint(&server, path))
        .header("accept", "application/json")
        .query(query)
        .send()
        .await
        .map_err(|error| request_error(&error, None))?;
    emit_response(response, json_output, None).await
}

pub async fn fetch_with_query(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    path: &str,
    query: &[(String, String)],
) -> Result<Value, AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .get(endpoint(&server, path))
        .header("accept", "application/json")
        .query(query)
        .send()
        .await
        .map_err(|error| request_error(&error, None))?;
    decode_response(response, None).await
}

pub(crate) struct ResourceResponse {
    envelope: Value,
    etag: String,
}

impl ResourceResponse {
    pub(crate) fn envelope(&self) -> &Value {
        &self.envelope
    }

    pub(crate) fn etag(&self) -> &str {
        &self.etag
    }
}

pub async fn fetch_resource(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    path: &str,
) -> Result<ResourceResponse, AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .get(endpoint(&server, path))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| request_error(&error, None))?;
    let etag = response
        .status()
        .is_success()
        .then(|| {
            response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .flatten();
    let envelope = decode_response(response, None).await?;
    let etag =
        etag.ok_or_else(|| AppError::internal("daemon resource response has no valid ETag"))?;
    Ok(ResourceResponse { envelope, etag })
}

pub(crate) struct ExistingResourceMutation<'a, T> {
    path: &'a str,
    if_match: &'a str,
    idempotency_key: &'a str,
    body: &'a T,
}

impl<'a, T> ExistingResourceMutation<'a, T> {
    pub(crate) const fn new(
        path: &'a str,
        if_match: &'a str,
        idempotency_key: &'a str,
        body: &'a T,
    ) -> Self {
        Self {
            path,
            if_match,
            idempotency_key,
            body,
        }
    }
}

pub async fn patch<T: Serialize>(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    mutation: ExistingResourceMutation<'_, T>,
    json_output: bool,
) -> Result<(), AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .patch(endpoint(&server, mutation.path))
        .header("accept", "application/json")
        .header("if-match", mutation.if_match)
        .header("idempotency-key", mutation.idempotency_key)
        .json(mutation.body)
        .send()
        .await
        .map_err(|error| request_error(&error, Some(mutation.idempotency_key)))?;
    emit_response(response, json_output, Some(mutation.idempotency_key)).await
}

pub async fn delete<T: Serialize>(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    mutation: ExistingResourceMutation<'_, T>,
    json_output: bool,
) -> Result<(), AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .delete(endpoint(&server, mutation.path))
        .header("accept", "application/json")
        .header("if-match", mutation.if_match)
        .header("idempotency-key", mutation.idempotency_key)
        .json(mutation.body)
        .send()
        .await
        .map_err(|error| request_error(&error, Some(mutation.idempotency_key)))?;
    emit_response(response, json_output, Some(mutation.idempotency_key)).await
}

pub async fn post_existing<T: Serialize>(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    mutation: ExistingResourceMutation<'_, T>,
    json_output: bool,
) -> Result<(), AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .post(endpoint(&server, mutation.path))
        .header("accept", "application/json")
        .header("if-match", mutation.if_match)
        .header("idempotency-key", mutation.idempotency_key)
        .json(mutation.body)
        .send()
        .await
        .map_err(|error| request_error(&error, Some(mutation.idempotency_key)))?;
    emit_response(response, json_output, Some(mutation.idempotency_key)).await
}

pub async fn post<T: Serialize>(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    path: &str,
    idempotency_key: &str,
    body: &T,
    json_output: bool,
) -> Result<(), AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .post(endpoint(&server, path))
        .header("accept", "application/json")
        .header("idempotency-key", idempotency_key)
        .json(body)
        .send()
        .await
        .map_err(|error| request_error(&error, Some(idempotency_key)))?;
    emit_response(response, json_output, Some(idempotency_key)).await
}

pub async fn post_batch<T: Serialize>(
    server_override: Option<String>,
    timeout_override_ms: Option<u64>,
    path: &str,
    idempotency_key: &str,
    body: &T,
    json_output: bool,
) -> Result<bool, AppError> {
    let (client, server) = client(server_override, timeout_override_ms)?;
    let response = client
        .post(endpoint(&server, path))
        .header("accept", "application/json")
        .header("idempotency-key", idempotency_key)
        .json(body)
        .send()
        .await
        .map_err(|error| request_error(&error, Some(idempotency_key)))?;
    let envelope = decode_response(response, Some(idempotency_key)).await?;
    let complete = envelope
        .pointer("/result/overall")
        .and_then(Value::as_str)
        .is_some_and(|overall| overall == "complete");
    emit_envelope(&envelope, json_output)?;
    Ok(complete)
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
        .map_err(|error| request_error(&error, None))?;
    emit_response(response, json_output, None).await
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

async fn emit_response(
    response: reqwest::Response,
    json_output: bool,
    idempotency_key: Option<&str>,
) -> Result<(), AppError> {
    let envelope = decode_response(response, idempotency_key).await?;
    emit_envelope(&envelope, json_output)
}

fn emit_envelope(envelope: &Value, json_output: bool) -> Result<(), AppError> {
    let rendered = if json_output {
        serde_json::to_vec(envelope)
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
    Ok(())
}

async fn decode_response(
    response: reqwest::Response,
    idempotency_key: Option<&str>,
) -> Result<Value, AppError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| request_error(&error, idempotency_key))?;
    let envelope = serde_json::from_slice::<Value>(&bytes)
        .map_err(|error| AppError::internal(format!("daemon returned invalid JSON: {error}")))?;
    if status.is_success() {
        return Ok(envelope);
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

fn request_error(error: &reqwest::Error, idempotency_key: Option<&str>) -> AppError {
    if error.is_timeout() {
        AppError::client_timeout(idempotency_key)
    } else {
        AppError::unavailable()
    }
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
