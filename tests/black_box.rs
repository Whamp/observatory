use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    _root: TempDir,
    runtime: PathBuf,
    storage: PathBuf,
    address: SocketAddr,
    child: Child,
}

impl Harness {
    fn start() -> Self {
        let root = supported_tempdir("temporary root");
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).expect("runtime directory");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
        let storage = root.path().join("data");
        let address = free_address();
        let child = daemon(&runtime, &storage, address)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn obs serve");
        let harness = Self {
            _root: root,
            runtime,
            storage,
            address,
            child,
        };
        harness.wait_ready();
        harness
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.address, path)
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(response) = reqwest::blocking::get(self.url("/api/v1/system/health"))
                && response.status().is_success()
            {
                let body: Value = response.json().expect("health JSON");
                assert_eq!(body["result"]["ready"], true);
                assert_eq!(body["result"]["startupReconciliation"], "complete");
                return;
            }
            thread::sleep(Duration::from_millis(30));
        }
        panic!("daemon did not become ready");
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn obs() -> Command {
    Command::new(env!("CARGO_BIN_EXE_obs"))
}

fn supported_tempdir(description: &str) -> TempDir {
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect(description)
}

fn free_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    drop(listener);
    address
}

fn daemon(runtime: &Path, storage: &Path, address: SocketAddr) -> Command {
    let mut command = obs();
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .arg("serve")
        .arg("--listen")
        .arg(address.to_string())
        .arg("--canonical-origin")
        .arg("https://desktop.greyhound-chinstrap.ts.net/")
        .arg("--storage")
        .arg(storage);
    command
}

fn cli(server: &str, args: &[&str]) -> Output {
    obs()
        .arg("--json")
        .arg("--server")
        .arg(server)
        .args(args)
        .output()
        .expect("run obs")
}

fn one_response_server(status: &str, body: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("response server");
    let address = listener.local_addr().expect("response server address");
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connection");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("request");
        stream.write_all(response.as_bytes()).expect("response");
    });
    (format!("http://{address}"), handle)
}

#[test]
fn unavailable_daemon_does_not_autostart() {
    let address = free_address();
    let output = cli(&format!("http://{address}"), &["system", "status"]);
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).expect("error envelope");
    assert_eq!(error["schemaVersion"], 1);
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["code"], "daemon_unavailable");

    let root = supported_tempdir("proposal root");
    let proposal = root.path().join("proposal.toml");
    fs::write(&proposal, "[server]\nlisten='127.0.0.1:3773'\n").expect("proposal");
    let validate = cli(
        &format!("http://{address}"),
        &[
            "system",
            "config",
            "validate",
            proposal.to_str().expect("path"),
        ],
    );
    assert_eq!(validate.status.code(), Some(5));
    assert!(validate.stdout.is_empty());
}

#[test]
fn daemon_failure_is_one_server_supplied_json_error() {
    let body = r#"{"schemaVersion":1,"ok":false,"error":{"code":"daemon_unavailable","message":"catalogue is offline","retryable":true,"details":{}}}"#;
    let (server, handle) = one_response_server("503 Service Unavailable", body);
    let output = cli(&server, &["system", "status"]);
    handle.join().expect("response server joined");

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr.split(|byte| *byte == b'\n').count(), 2);
    let emitted: Value = serde_json::from_slice(&output.stderr).expect("single error envelope");
    let supplied: Value = serde_json::from_str(body).expect("supplied error envelope");
    assert_eq!(emitted, supplied);
}

#[test]
fn daemon_exposes_empty_authority_configuration_and_shell() {
    let harness = Harness::start();
    assert!(harness.storage.join("catalogue.sqlite").is_file());
    assert_eq!(
        fs::metadata(&harness.storage)
            .expect("storage metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(harness.storage.join("catalogue.sqlite"))
            .expect("catalogue metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    for path in [
        "/api/v1/system/health",
        "/api/v1/system/status",
        "/api/v1/system/configuration",
    ] {
        let response = reqwest::blocking::get(harness.url(path)).expect("API response");
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body: Value = response.json().expect("API envelope");
        assert_eq!(body["schemaVersion"], 1);
        assert_eq!(body["ok"], true);
    }

    let status = cli(
        &format!("http://{}", harness.address),
        &["system", "status"],
    );
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).expect("status envelope");
    assert_eq!(status["result"]["catalogue"]["projects"], 0);
    assert_eq!(status["result"]["catalogue"]["artifacts"], 0);
    assert_eq!(status["result"]["catalogue"]["services"], 0);
    assert_eq!(status["result"]["health"], "healthy");
    assert_eq!(status["result"]["partial"], false);
    let check_ids = status["result"]["checks"]
        .as_array()
        .expect("status checks")
        .iter()
        .map(|check| check["id"].as_str().expect("check id"))
        .collect::<Vec<_>>();
    assert_eq!(
        check_ids,
        [
            "sqlite.open",
            "sqlite.application",
            "sqlite.schema",
            "sqlite.wal",
            "storage.intents",
            "revision.path",
            "storage.staging",
            "storage.quarantine",
            "storage.backup_leases",
            "storage.cleanup",
            "storage.filesystem",
            "storage.capacity",
        ]
    );
    for check in status["result"]["checks"].as_array().expect("checks") {
        for field in [
            "id",
            "status",
            "state",
            "category",
            "message",
            "retryable",
            "scope",
            "observedAt",
            "durationMs",
            "details",
        ] {
            assert!(check.get(field).is_some(), "missing status field {field}");
        }
    }
    assert_eq!(status["result"]["policy"]["applicationId"], 0x4f42_5356_u64);
    assert_eq!(status["result"]["policy"]["userVersion"], 1);
    assert_eq!(status["result"]["policy"]["foreignKeys"], true);
    assert_eq!(status["result"]["policy"]["journalMode"], "wal");
    assert_eq!(status["result"]["policy"]["synchronous"], "FULL");
    assert_eq!(status["result"]["policy"]["busyTimeoutMs"], 5000);
    assert_eq!(status["result"]["policy"]["strictTables"], true);

    let human_status = obs()
        .arg("--server")
        .arg(format!("http://{}", harness.address))
        .args(["system", "status"])
        .output()
        .expect("human status");
    assert!(human_status.status.success());
    assert!(!human_status.stdout.starts_with(b"{"));
    assert!(String::from_utf8_lossy(&human_status.stdout).contains("healthy"));

    let configuration: Value = reqwest::blocking::get(harness.url("/api/v1/system/configuration"))
        .expect("configuration response")
        .json()
        .expect("configuration JSON");
    assert_eq!(configuration["result"]["storage"]["path"], "<redacted>");
    assert!(
        !configuration
            .to_string()
            .contains(harness.storage.to_str().expect("storage path"))
    );

    let no_redirect = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client");
    let redirect = no_redirect
        .get(harness.url("/"))
        .send()
        .expect("root redirect");
    assert_eq!(redirect.status(), 308);
    assert_eq!(
        redirect.headers()["location"],
        "https://desktop.greyhound-chinstrap.ts.net/ui/"
    );
    assert_eq!(redirect.headers()["cache-control"], "no-store");
    let root = reqwest::blocking::get(harness.url("/ui/")).expect("UI response");
    assert_eq!(root.status(), 200);
    assert_eq!(root.headers()["cache-control"], "no-store");
    let html = root.text().expect("shell HTML");
    assert!(html.contains("Projects"));
    assert!(html.contains("No projects yet"));

    let css_path = html
        .split("href=\"")
        .find_map(|part| {
            part.strip_prefix("/_static/")
                .and_then(|rest| rest.split('\"').next())
        })
        .map(|rest| format!("/_static/{rest}"))
        .expect("versioned stylesheet");
    let css = reqwest::blocking::get(harness.url(&css_path)).expect("stylesheet");
    assert_eq!(css.status(), 200);
    assert_eq!(
        css.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert!(
        css.headers()["etag"]
            .to_str()
            .expect("ETag")
            .starts_with('"')
    );
    let javascript =
        reqwest::blocking::get(harness.url("/_static/empty-ledger-v1/app.js")).expect("ES module");
    assert_eq!(javascript.status(), 200);
    assert_eq!(
        javascript.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert!(
        javascript.headers()["etag"]
            .to_str()
            .expect("ETag")
            .starts_with('"')
    );
    let javascript_etag = javascript.headers()["etag"].clone();
    let not_modified = reqwest::blocking::Client::new()
        .get(harness.url("/_static/empty-ledger-v1/app.js"))
        .header("if-none-match", javascript_etag)
        .send()
        .expect("conditional ES module");
    assert_eq!(not_modified.status(), 304);
    assert_eq!(
        not_modified.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );

    let malformed = reqwest::blocking::Client::new()
        .post(harness.url("/api/v1/system/configuration/validate"))
        .header("content-type", "application/json")
        .body("{")
        .send()
        .expect("malformed proposal response");
    assert_eq!(malformed.status(), 400);
    assert_eq!(malformed.headers()["cache-control"], "no-store");
    let malformed: Value = malformed.json().expect("malformed error envelope");
    assert_eq!(malformed["schemaVersion"], 1);
    assert_eq!(malformed["ok"], false);

    for path in ["/unknown", "/api", "/api/v1/system/setup"] {
        assert_eq!(
            reqwest::blocking::get(harness.url(path))
                .expect("404 response")
                .status(),
            404
        );
    }
}

#[test]
fn all_configuration_fields_follow_cli_environment_toml_precedence() {
    let root = supported_tempdir("temporary root");
    let runtime = root.path().join("runtime");
    fs::create_dir(&runtime).expect("runtime directory");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
    let config_home = root.path().join("config");
    fs::create_dir_all(config_home.join("observatory")).expect("config directory");
    let toml_storage = root.path().join("toml-data");
    fs::write(
        config_home.join("observatory/config.toml"),
        format!(
            "[server]\nlisten='127.0.0.1:1'\ncanonical_origin='https://toml.example/'\n\
             [storage]\npath='{}'\nmax_stored_bytes=1\nmax_live_artifacts=1\n\
             [service]\nteardown_timeout_ms=1001\n\
             [client]\nserver='http://127.0.0.1:1'\ntimeout_ms=1\n",
            toml_storage.display()
        ),
    )
    .expect("configuration");
    let address = free_address();
    let cli_storage = root.path().join("cli-data");
    let mut command = obs();
    command
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("OBS_LISTEN", "127.0.0.1:2")
        .env("OBS_CANONICAL_ORIGIN", "https://environment.example/")
        .env("OBS_STORAGE", root.path().join("environment-data"))
        .env("OBS_MAX_STORED_BYTES", "2")
        .env("OBS_MAX_LIVE_ARTIFACTS", "2")
        .env("OBS_TEARDOWN_TIMEOUT_MS", "2000")
        .env("OBS_SERVER", "http://127.0.0.1:2")
        .env("OBS_CLIENT_TIMEOUT_MS", "2000")
        .args([
            "serve",
            "--listen",
            &address.to_string(),
            "--canonical-origin",
            "https://cli.example/",
            "--storage",
            cli_storage.to_str().expect("storage path"),
            "--max-stored-bytes",
            "3",
            "--max-live-artifacts",
            "3",
            "--teardown-timeout",
            "3000ms",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("daemon");
    let mut harness = Harness {
        _root: root,
        runtime,
        storage: cli_storage,
        address,
        child,
    };
    harness.wait_ready();
    let configuration: Value = reqwest::blocking::get(harness.url("/api/v1/system/configuration"))
        .expect("configuration response")
        .json()
        .expect("configuration JSON");
    let result = &configuration["result"];
    assert_eq!(result["server"]["listen"], address.to_string());
    assert_eq!(result["server"]["canonicalOrigin"], "https://cli.example/");
    assert_eq!(result["storage"]["path"], "<redacted>");
    assert!(harness.storage.join("catalogue.sqlite").is_file());
    assert_eq!(result["storage"]["maxStoredBytes"], 3);
    assert_eq!(result["storage"]["maxLiveArtifacts"], 3);
    assert_eq!(result["service"]["teardownTimeoutMs"], 3000);
    assert_eq!(result["client"]["server"], "http://127.0.0.1:2");
    assert_eq!(result["client"]["timeoutMs"], 2000);
    let _ = harness.child.kill();
}

#[test]
fn validation_is_content_only_ordered_and_non_mutating() {
    let harness = Harness::start();
    let proposal = harness._root.path().join("proposal.toml");
    fs::write(&proposal, "[server]\nlisten = '0.0.0.0:99'\n").expect("proposal");
    let before = fs::metadata(harness.storage.join("catalogue.sqlite"))
        .expect("catalogue metadata")
        .modified()
        .expect("mtime");
    let output = cli(
        &format!("http://{}", harness.address),
        &[
            "system",
            "config",
            "validate",
            proposal.to_str().expect("path"),
        ],
    );
    assert!(output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout).expect("validation envelope");
    assert_eq!(body["result"]["valid"], false);
    assert_eq!(body["result"]["checks"][0]["name"], "parse");
    assert_eq!(body["result"]["checks"][1]["name"], "schema");
    assert_eq!(body["result"]["checks"][2]["name"], "semantic");
    thread::sleep(Duration::from_millis(20));
    let after = fs::metadata(harness.storage.join("catalogue.sqlite"))
        .expect("catalogue metadata")
        .modified()
        .expect("mtime");
    assert_eq!(before, after);

    let symlink = harness._root.path().join("proposal-link.toml");
    std::os::unix::fs::symlink(&proposal, &symlink).expect("proposal symlink");
    let rejected = cli(
        &format!("http://{}", harness.address),
        &[
            "system",
            "config",
            "validate",
            symlink.to_str().expect("path"),
        ],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(rejected.stdout.is_empty());

    let proposal_directory = harness._root.path().join("proposal-directory");
    fs::create_dir(&proposal_directory).expect("proposal directory");
    fs::write(proposal_directory.join("nested.toml"), "").expect("nested proposal");
    let directory_link = harness._root.path().join("proposal-directory-link");
    std::os::unix::fs::symlink(&proposal_directory, &directory_link)
        .expect("proposal directory symlink");
    let parent_rejected = cli(
        &format!("http://{}", harness.address),
        &[
            "system",
            "config",
            "validate",
            directory_link
                .join("nested.toml")
                .to_str()
                .expect("nested path"),
        ],
    );
    assert_eq!(parent_rejected.status.code(), Some(2));
    assert!(parent_rejected.stdout.is_empty());
}

#[test]
fn health_reflects_storage_check_failures() {
    let harness = Harness::start();
    fs::write(harness.storage.join("staging/unclassified"), b"partial")
        .expect("unclassified staging evidence");
    let health: Value = reqwest::blocking::get(harness.url("/api/v1/system/health"))
        .expect("health response")
        .json()
        .expect("health JSON");
    assert_eq!(health["ok"], true);
    assert_eq!(health["result"]["ready"], false);
    assert_eq!(health["result"]["storageHealth"], "unhealthy");
}

#[test]
fn startup_quarantines_unreferenced_bytes_before_readiness() {
    let mut harness = Harness::start();
    let pid = harness.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("SIGTERM")
            .success()
    );
    assert!(harness.child.wait().expect("stopped daemon").success());

    fs::write(harness.storage.join("staging/abandoned"), b"partial")
        .expect("abandoned staging evidence");
    fs::create_dir(harness.storage.join("revisions/unreferenced"))
        .expect("unreferenced Revision evidence");
    fs::write(
        harness.storage.join("revisions/unreferenced/index.html"),
        b"uncommitted",
    )
    .expect("unreferenced bytes");

    harness.child = daemon(&harness.runtime, &harness.storage, harness.address)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("restart daemon");
    harness.wait_ready();
    assert_eq!(
        fs::read_dir(harness.storage.join("staging"))
            .expect("staging")
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(harness.storage.join("revisions"))
            .expect("Revisions")
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(harness.storage.join("quarantine"))
            .expect("quarantine")
            .count(),
        2
    );
}

#[test]
fn rejects_catalogue_identity_mismatch_before_readiness() {
    let mut harness = Harness::start();
    let pid = harness.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("SIGTERM")
            .success()
    );
    assert!(harness.child.wait().expect("stopped daemon").success());
    let connection = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("open catalogue fixture");
    connection
        .execute_batch("PRAGMA application_id=1;")
        .expect("change application identity");
    drop(connection);

    let output = daemon(&harness.runtime, &harness.storage, harness.address)
        .output()
        .expect("mismatched catalogue attempt");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("wrong application identity"));
    assert!(TcpListener::bind(harness.address).is_ok());
}

#[test]
fn rejects_symlinked_runtime_authority_directory() {
    let root = supported_tempdir("temporary root");
    let runtime = root.path().join("runtime");
    let outside = root.path().join("outside");
    fs::create_dir(&runtime).expect("runtime");
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
    fs::create_dir(&outside).expect("outside");
    std::os::unix::fs::symlink(&outside, runtime.join("observatory"))
        .expect("runtime authority symlink");
    let storage = root.path().join("must-not-exist");
    let output = daemon(&runtime, &storage, free_address())
        .output()
        .expect("symlinked authority attempt");
    assert!(!output.status.success());
    assert!(!storage.exists());
    assert!(!outside.join("daemon.lock").exists());
}

#[test]
fn rejects_second_daemon_and_restarts_after_sigterm() {
    let mut harness = Harness::start();
    let alternate_storage = harness._root.path().join("must-not-exist");
    let second = daemon(&harness.runtime, &alternate_storage, free_address())
        .output()
        .expect("second daemon");
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("daemon_already_running"));
    assert!(!alternate_storage.exists());

    let pid = harness.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-HUP", &pid])
            .status()
            .expect("SIGHUP")
            .success()
    );
    harness.wait_ready();

    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("SIGTERM")
            .success()
    );
    assert!(harness.child.wait().expect("graceful exit").success());
    harness.child = daemon(&harness.runtime, &harness.storage, harness.address)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("restart daemon");
    harness.wait_ready();
}
