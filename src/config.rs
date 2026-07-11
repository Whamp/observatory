use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::AppError;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveConfiguration {
    pub server: ServerConfiguration,
    pub storage: StorageConfiguration,
    pub service: ServiceConfiguration,
    pub client: ClientConfiguration,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfiguration {
    pub listen: String,
    pub canonical_origin: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageConfiguration {
    pub path: PathBuf,
    pub max_stored_bytes: u64,
    pub max_live_artifacts: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceConfiguration {
    pub teardown_timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfiguration {
    pub server: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactedConfiguration<'a> {
    pub server: &'a ServerConfiguration,
    pub storage: RedactedStorageConfiguration,
    pub service: &'a ServiceConfiguration,
    pub client: &'a ClientConfiguration,
    pub restart_required: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactedStorageConfiguration {
    pub path: &'static str,
    pub max_stored_bytes: u64,
    pub max_live_artifacts: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ServeOverrides {
    pub listen: Option<String>,
    pub canonical_origin: Option<String>,
    pub storage: Option<PathBuf>,
    pub max_stored_bytes: Option<u64>,
    pub max_live_artifacts: Option<u64>,
    pub teardown_timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigurationFile {
    server: Option<ServerFile>,
    storage: Option<StorageFile>,
    service: Option<ServiceFile>,
    client: Option<ClientFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    listen: Option<String>,
    canonical_origin: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageFile {
    path: Option<PathBuf>,
    max_stored_bytes: Option<u64>,
    max_live_artifacts: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceFile {
    teardown_timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientFile {
    server: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub checks: Vec<ValidationCheck>,
}

#[derive(Debug, Serialize)]
pub struct ValidationCheck {
    pub name: &'static str,
    pub status: &'static str,
    pub errors: Vec<String>,
}

impl EffectiveConfiguration {
    pub fn redacted(&self) -> RedactedConfiguration<'_> {
        RedactedConfiguration {
            server: &self.server,
            storage: RedactedStorageConfiguration {
                path: "<redacted>",
                max_stored_bytes: self.storage.max_stored_bytes,
                max_live_artifacts: self.storage.max_live_artifacts,
            },
            service: &self.service,
            client: &self.client,
            restart_required: false,
        }
    }

    pub fn load(overrides: &ServeOverrides) -> Result<Self, AppError> {
        let mut effective = Self::defaults()?;
        let path = configuration_path()?;
        if path.exists() {
            let content = fs::read_to_string(&path).map_err(|error| {
                AppError::usage(format!("cannot read {}: {error}", path.display()))
            })?;
            let file = toml::from_str::<ConfigurationFile>(&content)
                .map_err(|error| AppError::usage(format!("invalid configuration: {error}")))?;
            effective.apply_file(file);
        }
        effective.apply_environment()?;
        effective.apply_overrides(overrides);
        effective.validate_semantics()?;
        Ok(effective)
    }

    pub fn client(server: Option<String>) -> Result<Self, AppError> {
        let mut effective = Self::load(&ServeOverrides::default())?;
        if let Some(server) = server {
            effective.client.server = server;
        }
        effective.validate_semantics()?;
        Ok(effective)
    }

    fn defaults() -> Result<Self, AppError> {
        let home = absolute_home()?;
        let storage = env::var_os("XDG_DATA_HOME").map_or_else(
            || home.join(".local/share/observatory"),
            |path| PathBuf::from(path).join("observatory"),
        );
        Ok(Self {
            server: ServerConfiguration {
                listen: "127.0.0.1:3773".into(),
                canonical_origin: "https://desktop.greyhound-chinstrap.ts.net/".into(),
            },
            storage: StorageConfiguration {
                path: storage,
                max_stored_bytes: 0,
                max_live_artifacts: 0,
            },
            service: ServiceConfiguration {
                teardown_timeout_ms: 30_000,
            },
            client: ClientConfiguration {
                server: "http://127.0.0.1:3773".into(),
                timeout_ms: 30_000,
            },
        })
    }

    fn apply_file(&mut self, file: ConfigurationFile) {
        if let Some(server) = file.server {
            replace(&mut self.server.listen, server.listen);
            replace(&mut self.server.canonical_origin, server.canonical_origin);
        }
        if let Some(storage) = file.storage {
            replace(&mut self.storage.path, storage.path);
            replace(&mut self.storage.max_stored_bytes, storage.max_stored_bytes);
            replace(
                &mut self.storage.max_live_artifacts,
                storage.max_live_artifacts,
            );
        }
        if let Some(service) = file.service {
            replace(
                &mut self.service.teardown_timeout_ms,
                service.teardown_timeout_ms,
            );
        }
        if let Some(client) = file.client {
            replace(&mut self.client.server, client.server);
            replace(&mut self.client.timeout_ms, client.timeout_ms);
        }
    }

    fn apply_environment(&mut self) -> Result<(), AppError> {
        env_string("OBS_LISTEN", &mut self.server.listen);
        env_string("OBS_CANONICAL_ORIGIN", &mut self.server.canonical_origin);
        if let Some(value) = env::var_os("OBS_STORAGE") {
            self.storage.path = PathBuf::from(value);
        }
        env_u64("OBS_MAX_STORED_BYTES", &mut self.storage.max_stored_bytes)?;
        env_u64(
            "OBS_MAX_LIVE_ARTIFACTS",
            &mut self.storage.max_live_artifacts,
        )?;
        env_u64(
            "OBS_TEARDOWN_TIMEOUT_MS",
            &mut self.service.teardown_timeout_ms,
        )?;
        env_string("OBS_SERVER", &mut self.client.server);
        env_u64("OBS_CLIENT_TIMEOUT_MS", &mut self.client.timeout_ms)
    }

    fn apply_overrides(&mut self, overrides: &ServeOverrides) {
        replace(&mut self.server.listen, overrides.listen.clone());
        replace(
            &mut self.server.canonical_origin,
            overrides.canonical_origin.clone(),
        );
        replace(&mut self.storage.path, overrides.storage.clone());
        replace(
            &mut self.storage.max_stored_bytes,
            overrides.max_stored_bytes,
        );
        replace(
            &mut self.storage.max_live_artifacts,
            overrides.max_live_artifacts,
        );
        replace(
            &mut self.service.teardown_timeout_ms,
            overrides.teardown_timeout_ms,
        );
    }

    fn validate_semantics(&self) -> Result<(), AppError> {
        let listen = self
            .server
            .listen
            .parse::<SocketAddr>()
            .map_err(|_| AppError::usage("server.listen must be a socket address"))?;
        if !listen.ip().is_loopback() {
            return Err(AppError::usage("server.listen must be loopback-only"));
        }
        validate_origin(&self.server.canonical_origin)?;
        if !self.storage.path.is_absolute() {
            return Err(AppError::usage("storage.path must be absolute"));
        }
        if !(1_000..=300_000).contains(&self.service.teardown_timeout_ms) {
            return Err(AppError::usage(
                "service.teardown_timeout_ms must be between 1000 and 300000",
            ));
        }
        validate_client_url(&self.client.server)?;
        if !(1..=3_600_000).contains(&self.client.timeout_ms) {
            return Err(AppError::usage(
                "client.timeout_ms must be between 1 and 3600000",
            ));
        }
        Ok(())
    }
}

pub fn validate_proposal(content: &str) -> ValidationResult {
    let mut checks = Vec::with_capacity(3);
    if let Err(error) = content.parse::<toml::Value>() {
        checks.push(check("parse", "failed", vec![error.to_string()]));
        checks.push(check("schema", "skipped", Vec::new()));
        checks.push(check("semantic", "skipped", Vec::new()));
        return ValidationResult {
            valid: false,
            checks,
        };
    }
    checks.push(check("parse", "passed", Vec::new()));
    let file = match toml::from_str::<ConfigurationFile>(content) {
        Ok(file) => file,
        Err(error) => {
            checks.push(check("schema", "failed", vec![error.to_string()]));
            checks.push(check("semantic", "skipped", Vec::new()));
            return ValidationResult {
                valid: false,
                checks,
            };
        }
    };
    checks.push(check("schema", "passed", Vec::new()));
    let semantic = EffectiveConfiguration::defaults().and_then(|mut config| {
        config.apply_file(file);
        config.validate_semantics()
    });
    match semantic {
        Ok(()) => {
            checks.push(check("semantic", "passed", Vec::new()));
            ValidationResult {
                valid: true,
                checks,
            }
        }
        Err(error) => {
            checks.push(check("semantic", "failed", vec![error.message]));
            ValidationResult {
                valid: false,
                checks,
            }
        }
    }
}

fn check(name: &'static str, status: &'static str, errors: Vec<String>) -> ValidationCheck {
    ValidationCheck {
        name,
        status,
        errors,
    }
}

fn replace<T>(target: &mut T, source: Option<T>) {
    if let Some(source) = source {
        *target = source;
    }
}

fn env_string(name: &str, target: &mut String) {
    if let Ok(value) = env::var(name) {
        *target = value;
    }
}

fn env_u64(name: &str, target: &mut u64) -> Result<(), AppError> {
    if let Ok(value) = env::var(name) {
        *target = value
            .parse()
            .map_err(|_| AppError::usage(format!("{name} must be an unsigned integer")))?;
    }
    Ok(())
}

fn validate_origin(value: &str) -> Result<(), AppError> {
    let url = Url::parse(value)
        .map_err(|_| AppError::usage("server.canonical_origin must be an absolute URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AppError::usage(
            "server.canonical_origin must be an absolute HTTPS origin ending in /",
        ));
    }
    Ok(())
}

fn validate_client_url(value: &str) -> Result<(), AppError> {
    let url = Url::parse(value)
        .map_err(|_| AppError::usage("client.server must be an absolute HTTP(S) URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::usage(
            "client.server must be an absolute credential-free HTTP(S) URL without a fragment",
        ));
    }
    Ok(())
}

fn absolute_home() -> Result<PathBuf, AppError> {
    let home = env::var_os("HOME").ok_or_else(|| AppError::usage("HOME is required"))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(AppError::usage("HOME must be absolute"));
    }
    Ok(home)
}

fn configuration_path() -> Result<PathBuf, AppError> {
    let directory = match env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => absolute_home()?.join(".config"),
    };
    if !directory.is_absolute() {
        return Err(AppError::usage("XDG_CONFIG_HOME must be absolute"));
    }
    Ok(directory.join(Path::new("observatory/config.toml")))
}
