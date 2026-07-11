use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
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
        Self::start_with(|_| {})
    }

    fn start_with(setup: impl FnOnce(&Path)) -> Self {
        Self::start_configured(setup, |_| {})
    }

    fn start_configured(setup: impl FnOnce(&Path), configure: impl FnOnce(&mut Command)) -> Self {
        let root = supported_tempdir("temporary root");
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).expect("runtime directory");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
        let storage = root.path().join("data");
        setup(&storage);
        let address = free_address();
        let mut process = daemon(&runtime, &storage, address);
        configure(&mut process);
        let child = process
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

    fn restart_configured(&mut self, configure: impl FnOnce(&mut Command)) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut process = daemon(&self.runtime, &self.storage, self.address);
        configure(&mut process);
        self.child = process
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("restart obs serve");
        self.wait_ready();
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

fn raw_get_status(address: SocketAddr, path: &str) -> u16 {
    let mut stream = TcpStream::connect(address).expect("raw HTTP connection");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .expect("raw HTTP request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("raw HTTP response");
    response
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .expect("raw HTTP status")
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

fn hidden_form_value(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let value = html
        .split_once(&marker)
        .and_then(|(_, remainder)| remainder.split_once('\"'))
        .map(|(value, _)| value)
        .expect("hidden form value");
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn seed_v2_catalogue(storage: &Path) {
    for directory in [
        storage.to_path_buf(),
        storage.join("staging"),
        storage.join("revisions"),
        storage.join("quarantine"),
        storage.join("backups"),
        storage.join("candidates"),
    ] {
        fs::create_dir_all(&directory).expect("v2 private layout");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("v2 private mode");
    }
    let connection =
        rusqlite::Connection::open(storage.join("catalogue.sqlite")).expect("v2 catalogue");
    connection
        .execute_batch(
            "PRAGMA application_id=1329746774;
             PRAGMA user_version=2;
             PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE projects (
               id TEXT PRIMARY KEY, record_version INTEGER NOT NULL CHECK(record_version > 0),
               canonical_directory TEXT NOT NULL, state TEXT NOT NULL CHECK(state IN ('live','gone')),
               title TEXT NOT NULL, slug TEXT NOT NULL, title_fold TEXT NOT NULL,
               search_text TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
               terminal_state TEXT, tombstoned_at TEXT, cause TEXT
             ) STRICT;
             CREATE UNIQUE INDEX projects_canonical_directory ON projects(canonical_directory);
             CREATE TABLE artifacts (id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects(id), record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
             CREATE TABLE services (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
             CREATE TABLE revisions (id TEXT PRIMARY KEY, artifact_id TEXT NOT NULL REFERENCES artifacts(id), state TEXT NOT NULL CHECK(state IN ('available','unavailable'))) STRICT;
             CREATE TABLE operation_intents (id TEXT PRIMARY KEY, kind TEXT NOT NULL, state TEXT NOT NULL, details_json TEXT NOT NULL) STRICT;
             CREATE TABLE backup_leases (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
             CREATE TABLE cleanup_runs (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
             CREATE TABLE audit_events (
               sequence INTEGER PRIMARY KEY, kind TEXT NOT NULL, details_json TEXT NOT NULL,
               at TEXT NOT NULL, actor TEXT NOT NULL, cause TEXT NOT NULL,
               resource_type TEXT NOT NULL, resource_id TEXT NOT NULL
             ) STRICT;
             CREATE TABLE idempotency_requests (
               key TEXT PRIMARY KEY, fingerprint TEXT NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('in_progress','completed')),
               status_code INTEGER, response_json TEXT, etag TEXT, completed_at TEXT
             ) STRICT;
             CREATE TABLE system_state (key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;",
        )
        .expect("v2 schema");
    connection
        .execute(
            "INSERT INTO system_state(key,value) VALUES ('cursor_secret',?1)",
            [vec![7_u8; 32]],
        )
        .expect("v2 cursor secret");
    connection
        .execute(
            "INSERT INTO idempotency_requests(
               key,fingerprint,state,status_code,response_json,etag,completed_at
             ) VALUES ('preserved-key','preserved-fingerprint','completed',200,'{}',NULL,'2026-01-01T00:00:00.000Z')",
            [],
        )
        .expect("v2 idempotency fixture");
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
fn migrates_empty_v1_catalogue_before_readiness() {
    let harness = Harness::start_with(|storage| {
        for directory in [
            storage.to_path_buf(),
            storage.join("staging"),
            storage.join("revisions"),
            storage.join("quarantine"),
            storage.join("backups"),
            storage.join("candidates"),
        ] {
            fs::create_dir_all(&directory).expect("v1 private layout");
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .expect("v1 private mode");
        }
        let connection =
            rusqlite::Connection::open(storage.join("catalogue.sqlite")).expect("v1 catalogue");
        connection
            .execute_batch(
                "PRAGMA application_id=1329746774;
                 PRAGMA user_version=1;
                 PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 CREATE TABLE projects (id TEXT PRIMARY KEY, record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
                 CREATE TABLE artifacts (id TEXT PRIMARY KEY, project_id TEXT REFERENCES projects(id), record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
                 CREATE TABLE services (id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id), record_version INTEGER NOT NULL CHECK(record_version > 0)) STRICT;
                 CREATE TABLE revisions (id TEXT PRIMARY KEY, artifact_id TEXT NOT NULL REFERENCES artifacts(id), state TEXT NOT NULL CHECK(state IN ('available','unavailable'))) STRICT;
                 CREATE TABLE operation_intents (id TEXT PRIMARY KEY, kind TEXT NOT NULL, state TEXT NOT NULL, details_json TEXT NOT NULL) STRICT;
                 CREATE TABLE backup_leases (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
                 CREATE TABLE cleanup_runs (id TEXT PRIMARY KEY, state TEXT NOT NULL) STRICT;
                 CREATE TABLE audit_events (sequence INTEGER PRIMARY KEY, kind TEXT NOT NULL, details_json TEXT NOT NULL) STRICT;",
            )
            .expect("v1 schema");
    });

    let status = cli(
        &format!("http://{}", harness.address),
        &["system", "status"],
    );
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["result"]["policy"]["userVersion"], 5);
    let backup_path = harness
        .storage
        .join("backups/schema-v1-pre-migration.sqlite");
    assert!(backup_path.is_file());
    let backup = rusqlite::Connection::open_with_flags(
        backup_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("migration backup");
    assert_eq!(
        backup
            .query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))
            .expect("backup application ID"),
        0x4f42_5356
    );
    assert_eq!(
        backup
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("backup schema version"),
        1
    );
    assert_eq!(
        backup
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .expect("backup quick check"),
        "ok"
    );
}

#[test]
fn migrates_v2_catalogue_for_project_tombstone_gates() {
    let harness = Harness::start_with(seed_v2_catalogue);
    let status = cli(
        &format!("http://{}", harness.address),
        &["system", "status"],
    );
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).expect("v5 status JSON");
    assert_eq!(status["result"]["policy"]["userVersion"], 5);

    let backup_path = harness
        .storage
        .join("backups/schema-v2-pre-migration.sqlite");
    assert!(backup_path.is_file());
    let backup = rusqlite::Connection::open_with_flags(
        backup_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("v2 migration backup");
    assert_eq!(
        backup
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("backup schema version"),
        2
    );
    assert_eq!(
        backup
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .expect("backup quick check"),
        "ok"
    );
    let v3_backup = rusqlite::Connection::open_with_flags(
        harness
            .storage
            .join("backups/schema-v3-pre-migration.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("v3 migration backup");
    assert_eq!(
        v3_backup
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("v3 backup schema version"),
        3
    );
    assert_eq!(
        v3_backup
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .expect("v3 backup quick check"),
        "ok"
    );
    let v4_backup = rusqlite::Connection::open_with_flags(
        harness
            .storage
            .join("backups/schema-v4-pre-migration.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("v4 migration backup");
    assert_eq!(
        v4_backup
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("v4 backup schema version"),
        4
    );

    let catalogue =
        rusqlite::Connection::open(harness.storage.join("catalogue.sqlite")).expect("v4 catalogue");
    let idempotency_schema: String = catalogue
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='idempotency_requests'",
            [],
            |row| row.get(0),
        )
        .expect("v4 idempotency schema");
    assert!(idempotency_schema.contains("failed_terminal"));
    let preserved_state: String = catalogue
        .query_row(
            "SELECT state FROM idempotency_requests WHERE key='preserved-key'",
            [],
            |row| row.get(0),
        )
        .expect("preserved idempotency row");
    assert_eq!(preserved_state, "completed");
    let service_state: i64 = catalogue
        .query_row(
            "SELECT count(*) FROM pragma_table_info('services') WHERE name='state'",
            [],
            |row| row.get(0),
        )
        .expect("Service state column");
    let operation_project: i64 = catalogue
        .query_row(
            "SELECT count(*) FROM pragma_table_info('operation_intents') WHERE name='project_id'",
            [],
            |row| row.get(0),
        )
        .expect("operation Project column");
    assert_eq!((service_state, operation_project), (1, 1));
}

#[test]
fn project_resolve_register_and_replay_share_one_strict_operation() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("Agent Notes");
    fs::create_dir(&project_directory).expect("Project directory");
    let canonical_directory = fs::canonicalize(&project_directory).expect("canonical Project");
    let client = reqwest::blocking::Client::new();

    let unregistered = client
        .get(harness.url("/api/v1/projects/resolve"))
        .query(&[("path", canonical_directory.to_str().expect("UTF-8 Project"))])
        .send()
        .expect("resolve unregistered Project");
    assert_eq!(unregistered.status(), 200);
    assert_eq!(unregistered.headers()["cache-control"], "no-store");
    let unregistered: Value = unregistered.json().expect("resolve JSON");
    assert_eq!(unregistered["result"]["status"], "unregistered");
    assert!(unregistered["result"]["project"].is_null());

    let request = serde_json::json!({
        "path": canonical_directory,
        "title": "Agent Notes",
        "slug": "Agent Notes"
    });
    let missing_key = client
        .post(harness.url("/api/v1/projects"))
        .json(&request)
        .send()
        .expect("registration without key");
    assert_eq!(missing_key.status(), 422);

    let created = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-project-register")
        .json(&request)
        .send()
        .expect("register Project");
    assert_eq!(created.status(), 201);
    assert_eq!(created.headers()["etag"], "\"rv-1\"");
    assert_eq!(created.headers()["cache-control"], "no-store");
    let location = created.headers()["location"]
        .to_str()
        .expect("registration Location")
        .to_owned();
    let created: Value = created.json().expect("Project JSON");
    let project = &created["result"];
    let id = project["id"].as_str().expect("Project ID");
    assert_eq!(id.len(), 26);
    assert!(
        id.bytes()
            .all(|byte| b"0123456789abcdefghjkmnpqrstvwxyz".contains(&byte))
    );
    assert_eq!(project["kind"], "project");
    assert_eq!(project["state"], "live");
    assert_eq!(project["recordVersion"], 1);
    assert_eq!(project["title"], "Agent Notes");
    assert_eq!(project["slug"], "agent-notes");
    assert_eq!(project["key"], format!("agent-notes~{id}"));
    assert_eq!(
        project["canonicalDirectory"],
        canonical_directory.to_str().expect("canonical UTF-8 path")
    );
    assert_eq!(
        project["apiUrl"],
        format!("https://desktop.greyhound-chinstrap.ts.net/api/v1/projects/{id}")
    );
    assert_eq!(location, project["apiUrl"]);
    assert_eq!(
        project["detailUrl"],
        format!("https://desktop.greyhound-chinstrap.ts.net/ui/projects/agent-notes~{id}/")
    );

    let replay = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-project-register")
        .json(&request)
        .send()
        .expect("replay registration");
    assert_eq!(replay.status(), 201);
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(replay.json::<Value>().expect("replay JSON"), created);

    let changed = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-project-register")
        .json(&serde_json::json!({
            "path": canonical_directory,
            "title": "Changed title",
            "slug": "Agent Notes"
        }))
        .send()
        .expect("changed replay");
    assert_eq!(changed.status(), 409);
    let changed: Value = changed.json().expect("changed replay JSON");
    assert_eq!(changed["error"]["code"], "idempotency_conflict");

    let duplicate = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-project-duplicate")
        .json(&request)
        .send()
        .expect("duplicate registration");
    assert_eq!(duplicate.status(), 409);
    let duplicate: Value = duplicate.json().expect("duplicate JSON");
    assert_eq!(duplicate["error"]["code"], "already_exists");

    let equivalent = project_directory.join("..").join("Agent Notes");
    let registered = client
        .get(harness.url("/api/v1/projects/resolve"))
        .query(&[("path", equivalent.to_str().expect("equivalent path"))])
        .send()
        .expect("resolve registered Project");
    let registered: Value = registered.json().expect("registered resolve JSON");
    assert_eq!(registered["result"]["status"], "registered");
    assert_eq!(registered["result"]["project"]["id"], id);

    let audit = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("audit catalogue");
    let event = audit
        .query_row(
            "SELECT actor, cause, resource_type, resource_id, details_json
             FROM audit_events WHERE cause='project_registered' ORDER BY sequence",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .expect("registration audit event");
    assert_eq!(event.0, "operator");
    assert_eq!(event.1, "project_registered");
    assert_eq!(event.2, "project");
    assert_eq!(event.3, id);
    assert!(
        !event
            .4
            .contains(project_directory.to_str().expect("Project path"))
    );
    let event_count: i64 = audit
        .query_row(
            "SELECT count(*) FROM audit_events WHERE cause='project_registered'",
            [],
            |row| row.get(0),
        )
        .expect("registration audit count");
    assert_eq!(event_count, 1);
}

#[test]
fn project_slugs_follow_normalization_fallback_and_route_grammar() {
    let harness = Harness::start();
    let client = reqwest::blocking::Client::new();
    let cases = [
        (
            "accented",
            serde_json::json!({
                "title": "Accented",
                "slug": "Crème 東京 / Résumé"
            }),
            "creme-resume".to_owned(),
        ),
        (
            "boundary",
            serde_json::json!({
                "title": "Boundary",
                "slug": format!("{} x", "a".repeat(47))
            }),
            "a".repeat(47),
        ),
        (
            "fallback",
            serde_json::json!({ "title": "東京" }),
            "project".to_owned(),
        ),
    ];
    for (index, (name, metadata, expected_slug)) in cases.into_iter().enumerate() {
        let directory = harness._root.path().join(name);
        fs::create_dir(&directory).expect("slug Project directory");
        let mut request = metadata;
        request["path"] = serde_json::json!(directory);
        let response = client
            .post(harness.url("/api/v1/projects"))
            .header("Idempotency-Key", format!("issue-22-slug-case-{index}"))
            .json(&request)
            .send()
            .expect("register slug Project");
        assert_eq!(response.status(), 201);
        let project: Value = response.json().expect("slug Project JSON");
        assert_eq!(project["result"]["slug"], expected_slug);
        let slug = project["result"]["slug"].as_str().expect("Project slug");
        assert!(slug.len() <= 48);
        assert!(!slug.ends_with('-'));
    }

    let invalid_directory = harness._root.path().join("invalid-slug");
    fs::create_dir(&invalid_directory).expect("invalid slug Project directory");
    let invalid = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-invalid-slug")
        .json(&serde_json::json!({
            "path": invalid_directory,
            "title": "Invalid slug",
            "slug": "東京"
        }))
        .send()
        .expect("register invalid slug Project");
    assert_eq!(invalid.status(), 422);
    assert_eq!(
        invalid.json::<Value>().expect("invalid slug JSON")["error"]["code"],
        "invalid_project_slug"
    );
}

#[test]
fn single_file_publish_creates_current_artifact_and_immutable_revision() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("artifact-project");
    fs::create_dir(&project_directory).expect("Artifact Project directory");
    let source = harness._root.path().join("agent-report.html");
    let source_bytes = b"<!doctype html><title>Agent Report</title><h1>Published exactly</h1>";
    fs::write(&source, source_bytes).expect("single-file Artifact source");
    let client = reqwest::blocking::Client::new();
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-register-project")
        .json(&serde_json::json!({
            "path": project_directory.to_str().expect("Artifact Project path"),
            "title": "Artifact Project",
            "slug": "artifact-project"
        }))
        .send()
        .expect("register Artifact Project")
        .json::<Value>()
        .expect("registered Artifact Project");
    let project = &registered["result"];
    let project_id = project["id"].as_str().expect("Project ID");
    let project_key = project["key"].as_str().expect("Project key");

    let published = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-publish-single-file")
        .json(&serde_json::json!({
            "source": {
                "path": source.to_str().expect("source path"),
                "callerWorkingDirectory": harness._root.path().to_str().expect("caller cwd")
            },
            "projectId": project_id,
            "entry": null,
            "title": null,
            "description": null,
            "slug": null,
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("publish single-file Artifact");
    assert_eq!(published.status(), 201);
    assert_eq!(published.headers()["cache-control"], "no-store");
    assert_eq!(published.headers()["etag"], "\"rv-1\"");
    let location = published.headers()["location"]
        .to_str()
        .expect("Artifact Location")
        .to_owned();
    let published: Value = published.json().expect("published Artifact result");
    assert_eq!(published["schemaVersion"], 1);
    assert_eq!(published["ok"], true);
    assert_eq!(published["result"]["operation"], "publish");
    let artifact = &published["result"]["artifact"];
    let revision = &published["result"]["revision"];
    let artifact_id = artifact["id"].as_str().expect("Artifact ID");
    let revision_id = revision["id"].as_str().expect("Revision ID");
    assert_eq!(artifact_id.len(), 26);
    assert_eq!(revision_id.len(), 26);
    assert_ne!(artifact_id, revision_id);
    assert_eq!(artifact["kind"], "artifact");
    assert_eq!(artifact["recordVersion"], 1);
    assert_eq!(artifact["state"], "live");
    assert_eq!(artifact["title"], "Agent Report");
    assert_eq!(artifact["description"], "");
    assert!(
        artifact["key"]
            .as_str()
            .expect("Artifact key")
            .starts_with("agent-report~")
    );
    assert_eq!(artifact["project"]["id"], project_id);
    assert_eq!(artifact["project"]["key"], project_key);
    assert_eq!(artifact["currentRevisionId"], revision_id);
    assert_eq!(artifact["retention"]["mode"], "default");
    assert_eq!(artifact["retention"]["ttlMs"], 2_592_000_000_u64);
    assert!(artifact["retention"]["expiresAt"].is_string());
    assert_eq!(artifact["retention"]["pinReason"], Value::Null);
    assert_eq!(artifact["retention"]["recoveryUntil"], Value::Null);
    assert_eq!(artifact["files"], 1);
    assert_eq!(artifact["logicalBytes"], source_bytes.len());
    assert_eq!(artifact["revisionCount"], 1);
    assert_eq!(artifact["apiUrl"], location);
    assert!(
        artifact["openUrl"]
            .as_str()
            .expect("Artifact Open URL")
            .ends_with('/')
    );
    assert!(
        artifact["detailUrl"]
            .as_str()
            .expect("Artifact detail URL")
            .ends_with('/')
    );

    assert_eq!(revision["kind"], "revision");
    assert_eq!(revision["artifactId"], artifact_id);
    assert_eq!(revision["state"], "current");
    assert_eq!(revision["entryPath"], "agent-report.html");
    assert_eq!(revision["entryMediaType"], "text/html");
    assert_eq!(revision["files"], 1);
    assert_eq!(revision["logicalBytes"], source_bytes.len());
    assert!(
        revision["manifestDigest"]
            .as_str()
            .expect("manifest digest")
            .starts_with("sha256:")
    );
    assert!(
        revision["apiUrl"]
            .as_str()
            .expect("Revision API URL")
            .ends_with(revision_id)
    );
    assert!(
        revision["openUrl"]
            .as_str()
            .expect("Revision Open URL")
            .ends_with('/')
    );

    let revision_directory = harness.storage.join("revisions").join(revision_id);
    let manifest_path = revision_directory.join("revision-manifest.json");
    let payload_path = revision_directory.join("content/agent-report.html");
    assert_eq!(
        fs::metadata(&revision_directory)
            .expect("Revision directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [&manifest_path, &payload_path] {
        assert_eq!(
            fs::metadata(path)
                .expect("Revision file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "path={}",
            path.display()
        );
    }
    let manifest_bytes = fs::read(&manifest_path).expect("Revision manifest bytes");
    let manifest: Value = serde_json::from_slice(&manifest_bytes).expect("Revision manifest JSON");
    assert_eq!(manifest["schemaVersion"], 1);
    assert_eq!(manifest["artifactId"], artifact_id);
    assert_eq!(manifest["revisionId"], revision_id);
    assert_eq!(manifest["entryPath"], "agent-report.html");
    assert_eq!(manifest["entryMediaType"], "text/html");
    assert_eq!(manifest["logicalBytes"], source_bytes.len());
    assert_eq!(manifest["members"][0]["path"], "agent-report.html");
    assert_eq!(
        manifest["members"][0]["digest"],
        format!("sha256:{:x}", Sha256::digest(source_bytes))
    );
    assert_eq!(
        revision["manifestDigest"],
        format!("sha256:{:x}", Sha256::digest(&manifest_bytes))
    );
    assert_eq!(
        fs::read(&payload_path).expect("stored payload"),
        source_bytes
    );
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("Publish evidence catalogue");
    let (operation_state, details): (String, String) = catalogue
        .query_row(
            "SELECT state,details_json FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("Publish operation evidence");
    assert_eq!(operation_state, "completed");
    assert!(!details.contains(source.to_str().expect("source path")));
    assert!(!details.contains(harness._root.path().to_str().expect("private root")));
    let audit_count: u64 = catalogue
        .query_row(
            "SELECT count(*) FROM audit_events WHERE kind='artifact_published' AND actor='operator' AND cause='publish' AND resource_id=?1",
            [artifact_id],
            |row| row.get(0),
        )
        .expect("Publish audit count");
    assert_eq!(audit_count, 1);

    let artifact_open = artifact["openUrl"].as_str().expect("Artifact Open URL");
    let revision_open = revision["openUrl"].as_str().expect("Revision Open URL");
    for open_url in [
        artifact_open.to_owned(),
        revision_open.to_owned(),
        format!("{artifact_open}agent-report.html"),
        format!("{revision_open}agent-report.html"),
    ] {
        let path = url::Url::parse(&open_url)
            .expect("canonical Open URL")
            .path()
            .to_owned();
        let response = client
            .get(harness.url(&path))
            .send()
            .expect("served Artifact bytes");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().expect("served bytes");
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        assert_eq!(headers["content-type"], "text/html");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(bytes.as_ref(), source_bytes);
        let head = client
            .head(harness.url(&path))
            .send()
            .expect("served Artifact HEAD");
        assert_eq!(head.status(), 200);
        assert_eq!(head.headers()["content-type"], "text/html");
        assert_eq!(head.headers()["x-content-type-options"], "nosniff");
        assert!(head.bytes().expect("HEAD body").is_empty());
    }
}

#[test]
fn directory_publish_uses_portable_metadata_and_serves_exact_nested_members() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("bundle-project");
    fs::create_dir(&project_directory).expect("bundle Project");
    let bundle = harness._root.path().join("portable-bundle");
    fs::create_dir_all(bundle.join("pages")).expect("bundle pages");
    fs::create_dir_all(bundle.join("assets")).expect("bundle assets");
    fs::write(
        bundle.join(".obs.json"),
        r#"{"schemaVersion":1,"entry":"pages/start.html","title":"Portable field guide","description":"A complete browser bundle"}"#,
    )
    .expect("portable metadata");
    fs::write(
        bundle.join("pages/start.html"),
        "<!doctype html><h1>Portable field guide</h1><script src=\"../assets/app.js\"></script>",
    )
    .expect("bundle entry");
    fs::write(
        bundle.join("index.html"),
        "<h1>Explicit entry</h1><link href=\"/root.css\">",
    )
    .expect("explicit bundle entry");
    fs::write(bundle.join("assets/app.js"), "window.bundleReady = true;\n").expect("bundle script");
    fs::write(bundle.join(".theme"), "night\n").expect("bundle dotfile");
    fs::create_dir(bundle.join("control")).expect("bundle control directory");
    fs::write(bundle.join("control/status.json"), "{\"ok\":true}")
        .expect("bundle control-named member");
    fs::write(bundle.join("café note.txt"), "unicode member").expect("bundle Unicode member");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-bundle-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Bundle Project"}))
        .send()
        .expect("register bundle Project")
        .json::<Value>()
        .expect("bundle Project JSON");
    let published = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-25-directory-publish")
        .json(&serde_json::json!({
            "source": {"path": bundle, "callerWorkingDirectory": harness._root.path()},
            "projectId": project["result"]["id"],
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("Publish directory bundle");
    assert_eq!(published.status(), 201);
    let published = published.json::<Value>().expect("directory Publish JSON");
    let artifact = &published["result"]["artifact"];
    let revision = &published["result"]["revision"];
    assert_eq!(artifact["title"], "Portable field guide");
    assert_eq!(artifact["description"], "A complete browser bundle");
    assert_eq!(artifact["files"], 6);
    assert_eq!(revision["entryPath"], "pages/start.html");
    assert_eq!(revision["entryMediaType"], "text/html");
    let expected_bytes = 84 + 46 + 27 + 6 + 11 + 14;
    assert_eq!(artifact["logicalBytes"], expected_bytes);
    let open_path = url::Url::parse(artifact["openUrl"].as_str().expect("bundle Open URL"))
        .expect("bundle Open URL parse")
        .path()
        .to_owned();
    let entry = client
        .get(harness.url(&open_path))
        .send()
        .expect("serve bundle entry");
    assert_eq!(entry.status(), 200);
    assert_eq!(
        entry.text().expect("bundle entry bytes"),
        "<!doctype html><h1>Portable field guide</h1><script src=\"../assets/app.js\"></script>"
    );
    let supporting = client
        .get(harness.url(&format!("{open_path}assets/app.js")))
        .send()
        .expect("serve nested supporting member");
    assert_eq!(supporting.status(), 200);
    assert_eq!(
        supporting.text().expect("supporting member bytes"),
        "window.bundleReady = true;\n"
    );
    let control_named = client
        .get(harness.url(&format!("{open_path}control/status.json")))
        .send()
        .expect("serve control-named member");
    assert_eq!(control_named.status(), 200);
    assert_eq!(
        control_named.text().expect("control-named bytes"),
        "{\"ok\":true}"
    );
    let unicode_member = client
        .get(harness.url(&format!("{open_path}caf%C3%A9%20note.txt")))
        .send()
        .expect("serve canonical Unicode member");
    assert_eq!(unicode_member.status(), 200);
    assert_eq!(
        unicode_member.text().expect("Unicode member bytes"),
        "unicode member"
    );
    let wrong_case = client
        .get(harness.url(&format!("{open_path}Assets/app.js")))
        .send()
        .expect("case-sensitive member miss");
    assert_eq!(wrong_case.status(), 404);
    for unsafe_path in ["assets%2Fapp.js", "assets%252Fapp.js", "assets//app.js"] {
        let rejected = client
            .get(harness.url(&format!("{open_path}{unsafe_path}")))
            .send()
            .expect("ambiguous nested member path");
        assert!(rejected.status().is_client_error(), "path={unsafe_path}");
    }
    for unsafe_path in ["assets/%2e/app.js", "assets/%2e%2e/app.js"] {
        let status = raw_get_status(harness.address, &format!("{open_path}{unsafe_path}"));
        assert!(
            (400..500).contains(&status),
            "path={unsafe_path}, status={status}"
        );
    }
    let revision_id = revision["id"].as_str().expect("bundle Revision ID");
    let revision_directory = harness.storage.join("revisions").join(revision_id);
    let manifest_bytes = fs::read(revision_directory.join("revision-manifest.json"))
        .expect("bundle Revision manifest");
    assert_eq!(
        revision["manifestDigest"],
        format!("sha256:{:x}", Sha256::digest(&manifest_bytes))
    );
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).expect("bundle Revision manifest JSON");
    assert_eq!(manifest["files"], 6);
    assert_eq!(manifest["logicalBytes"], expected_bytes);
    assert_eq!(
        manifest["members"]
            .as_array()
            .expect("bundle manifest members")
            .iter()
            .map(|member| member["path"].as_str().expect("bundle member path"))
            .collect::<Vec<_>>(),
        vec![
            ".theme",
            "assets/app.js",
            "café note.txt",
            "control/status.json",
            "index.html",
            "pages/start.html"
        ]
    );
    fs::write(
        revision_directory.join("content").join("unmanifested.txt"),
        "must not serve",
    )
    .expect("seed unmanifested immutable member");
    let unmanifested = client
        .get(harness.url(&format!("{open_path}unmanifested.txt")))
        .send()
        .expect("request unmanifested member");
    assert_eq!(unmanifested.status(), 404);
    let script_path = revision_directory.join("content/assets/app.js");
    fs::write(&script_path, "tampered bytes\n").expect("tamper immutable member");
    let tampered_member = client
        .get(harness.url(&format!("{open_path}assets/app.js")))
        .send()
        .expect("request tampered immutable member");
    assert_eq!(tampered_member.status(), 500);
    fs::write(&script_path, "window.bundleReady = true;\n").expect("restore immutable member");
    let mut changed_manifest = manifest.clone();
    changed_manifest["members"]
        .as_array_mut()
        .expect("mutable manifest members")
        .push(serde_json::json!({
            "path": "unmanifested.txt",
            "size": 14,
            "digest": format!("sha256:{:x}", Sha256::digest(b"must not serve"))
        }));
    changed_manifest["files"] = serde_json::json!(7);
    changed_manifest["logicalBytes"] = serde_json::json!(expected_bytes + 14);
    fs::write(
        revision_directory.join("revision-manifest.json"),
        serde_jcs::to_vec(&changed_manifest).expect("canonical changed manifest"),
    )
    .expect("tamper Revision manifest");
    let tampered_manifest = client
        .get(harness.url(&format!("{open_path}unmanifested.txt")))
        .send()
        .expect("request manifest-authorized extra member");
    assert_eq!(tampered_manifest.status(), 500);
    fs::write(
        revision_directory.join("revision-manifest.json"),
        &manifest_bytes,
    )
    .expect("restore Revision manifest");
    let metadata = client
        .get(harness.url(&format!("{open_path}.obs.json")))
        .send()
        .expect("portable metadata is consumed");
    assert_eq!(metadata.status(), 404);
    assert_eq!(
        published["result"]["warnings"][0]["code"],
        "root_relative_reference"
    );
    assert_eq!(published["result"]["warnings"][0]["member"], "index.html");

    let explicit = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-25-explicit-entry")
        .json(&serde_json::json!({
            "source": {"path": bundle, "callerWorkingDirectory": harness._root.path()},
            "projectId": project["result"]["id"],
            "entry": "index.html",
            "title": "Operator title",
            "description": "Operator description",
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("Publish explicit-entry bundle");
    assert_eq!(explicit.status(), 201);
    let explicit = explicit
        .json::<Value>()
        .expect("explicit-entry Publish JSON");
    assert_eq!(explicit["result"]["revision"]["entryPath"], "index.html");
    assert_eq!(explicit["result"]["artifact"]["title"], "Operator title");
    assert_eq!(
        explicit["result"]["artifact"]["description"],
        "Operator description"
    );
}

#[test]
fn directory_publish_rejects_unsafe_trees_metadata_and_entry_selection() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("bundle-validation-project");
    fs::create_dir(&project_directory).expect("bundle validation Project");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-validation-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Bundle Validation"}))
        .send()
        .expect("register bundle validation Project")
        .json::<Value>()
        .expect("bundle validation Project JSON");
    let project_id = project["result"]["id"].clone();

    let missing_entry = harness._root.path().join("missing-entry-bundle");
    fs::create_dir(&missing_entry).expect("missing-entry bundle");
    fs::write(missing_entry.join("notes.txt"), "notes").expect("missing-entry member");

    let malformed_metadata = harness._root.path().join("malformed-metadata-bundle");
    fs::create_dir(&malformed_metadata).expect("malformed metadata bundle");
    fs::write(malformed_metadata.join("index.html"), "<p>entry</p>")
        .expect("malformed metadata entry");
    fs::write(
        malformed_metadata.join(".obs.json"),
        r#"{"schemaVersion":1,"unexpected":true}"#,
    )
    .expect("malformed portable metadata");

    let unsupported_entry = harness._root.path().join("unsupported-entry-bundle");
    fs::create_dir(&unsupported_entry).expect("unsupported entry bundle");
    fs::write(unsupported_entry.join("app.css"), "body{}").expect("unsupported entry member");

    let symlink_bundle = harness._root.path().join("symlink-bundle");
    fs::create_dir(&symlink_bundle).expect("symlink bundle");
    fs::write(symlink_bundle.join("index.html"), "<p>entry</p>").expect("symlink entry");
    symlink("/etc/hosts", symlink_bundle.join("outside.txt")).expect("bundle symlink");

    let hardlink_bundle = harness._root.path().join("hardlink-bundle");
    fs::create_dir(&hardlink_bundle).expect("hardlink bundle");
    fs::write(hardlink_bundle.join("index.html"), "<p>entry</p>").expect("hardlink entry");
    fs::hard_link(
        hardlink_bundle.join("index.html"),
        hardlink_bundle.join("duplicate.html"),
    )
    .expect("bundle hard link");

    let unsafe_entry_bundle = harness._root.path().join("unsafe-entry-bundle");
    fs::create_dir(&unsafe_entry_bundle).expect("unsafe entry bundle");
    fs::write(unsafe_entry_bundle.join("index.html"), "<p>entry</p>").expect("unsafe entry file");

    let special_bundle = harness._root.path().join("special-bundle");
    fs::create_dir(&special_bundle).expect("special bundle");
    fs::write(special_bundle.join("index.html"), "<p>entry</p>").expect("special entry");
    let _socket = UnixListener::bind(special_bundle.join("agent.sock")).expect("bundle socket");

    for (ordinal, source, entry, expected_code) in [
        (1, &missing_entry, None, "invalid_entry"),
        (2, &malformed_metadata, None, "invalid_metadata"),
        (
            3,
            &unsupported_entry,
            Some("app.css"),
            "unsupported_entry_media",
        ),
        (4, &symlink_bundle, None, "unsafe_source"),
        (5, &hardlink_bundle, None, "unsafe_source"),
        (6, &special_bundle, None, "unsafe_source"),
        (
            7,
            &unsafe_entry_bundle,
            Some("../index.html"),
            "invalid_source",
        ),
    ] {
        let response = client
            .post(harness.url("/api/v1/artifacts"))
            .header(
                "Idempotency-Key",
                format!("issue-25-invalid-bundle-{ordinal}"),
            )
            .json(&serde_json::json!({
                "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
                "projectId": project_id,
                "entry": entry,
                "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
            }))
            .send()
            .expect("invalid directory Publish");
        assert!(response.status().is_client_error(), "ordinal={ordinal}");
        let body = response.json::<Value>().expect("invalid bundle JSON");
        assert_eq!(body["error"]["code"], expected_code, "ordinal={ordinal}");
        assert!(
            !body
                .to_string()
                .contains(harness._root.path().to_str().expect("private root")),
            "ordinal={ordinal} leaked source path"
        );
    }

    let fallback = harness._root.path().join("fallback-bundle");
    fs::create_dir(&fallback).expect("fallback bundle");
    fs::write(fallback.join("index.html"), "<p>fallback</p>").expect("fallback entry");
    let fallback = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-25-index-fallback")
        .json(&serde_json::json!({
            "source": {"path": fallback, "callerWorkingDirectory": harness._root.path()},
            "projectId": project_id,
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("index fallback Publish");
    assert_eq!(fallback.status(), 201);
    let fallback = fallback.json::<Value>().expect("index fallback JSON");
    assert_eq!(fallback["result"]["revision"]["entryPath"], "index.html");
    assert_eq!(fallback["result"]["artifact"]["title"], "fallback-bundle");
}

#[test]
fn directory_publish_accepts_every_browser_entry_media_family() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("bundle-media-project");
    fs::create_dir(&project_directory).expect("bundle media Project");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-media-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Bundle Media"}))
        .send()
        .expect("register bundle media Project")
        .json::<Value>()
        .expect("bundle media Project JSON");
    for (ordinal, name, media_type, bytes) in [
        (1, "entry.txt", "text/plain", b"plain text".as_slice()),
        (2, "entry.png", "image/png", b"png bytes".as_slice()),
        (3, "entry.mp3", "audio/mpeg", b"audio bytes".as_slice()),
        (4, "entry.mp4", "video/mp4", b"video bytes".as_slice()),
        (
            5,
            "entry.pdf",
            "application/pdf",
            b"%PDF fixture".as_slice(),
        ),
        (
            6,
            "entry.json",
            "application/json",
            br#"{"ok":true}"#.as_slice(),
        ),
        (7, "entry.md", "text/markdown", b"# Markdown".as_slice()),
    ] {
        let bundle = harness._root.path().join(format!("media-{ordinal}"));
        fs::create_dir(&bundle).expect("media bundle");
        fs::write(bundle.join(name), bytes).expect("media entry bytes");
        fs::write(bundle.join("support.js"), "supporting JavaScript")
            .expect("media supporting file");
        let published = client
            .post(harness.url("/api/v1/artifacts"))
            .header("Idempotency-Key", format!("issue-25-media-{ordinal}"))
            .json(&serde_json::json!({
                "source": {"path": bundle, "callerWorkingDirectory": harness._root.path()},
                "projectId": project["result"]["id"],
                "entry": name,
                "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
            }))
            .send()
            .expect("Publish media bundle");
        assert_eq!(published.status(), 201, "name={name}");
        let published = published.json::<Value>().expect("media Publish JSON");
        assert_eq!(
            published["result"]["revision"]["entryMediaType"],
            media_type
        );
        let open_path = url::Url::parse(
            published["result"]["artifact"]["openUrl"]
                .as_str()
                .expect("media Open URL"),
        )
        .expect("media Open URL parse")
        .path()
        .to_owned();
        assert_eq!(
            client
                .get(harness.url(&open_path))
                .send()
                .expect("serve media entry")
                .bytes()
                .expect("media response bytes")
                .as_ref(),
            bytes,
            "name={name}"
        );
    }
}

#[test]
fn directory_publish_detects_member_and_tree_mutation_before_visibility() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("bundle-race-project");
    fs::create_dir(&project_directory).expect("bundle race Project");
    let bundle = harness._root.path().join("changing-bundle");
    fs::create_dir(&bundle).expect("changing bundle");
    fs::write(bundle.join("00-index.html"), "<h1>Stable entry</h1>").expect("bundle race entry");
    let changing = bundle.join("changing.bin");
    let mut changing_file = fs::File::create(&changing).expect("changing bundle member");
    changing_file
        .set_len(32 * 1024 * 1024)
        .expect("size changing bundle member");
    changing_file
        .write_all(b"bundle race")
        .expect("initialize changing bundle member");
    changing_file
        .sync_all()
        .expect("sync changing bundle member");
    drop(changing_file);
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-bundle-race-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Bundle Race"}))
        .send()
        .expect("register bundle race Project")
        .json::<Value>()
        .expect("bundle race Project JSON");
    let mutating = Arc::new(AtomicBool::new(true));
    let writer_flag = mutating.clone();
    let writer_path = changing.clone();
    let writer = thread::spawn(move || {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(writer_path)
            .expect("open changing bundle writer");
        let mut byte = 0_u8;
        while writer_flag.load(Ordering::Acquire) {
            file.seek(SeekFrom::End(-1))
                .expect("seek changing bundle member");
            file.write_all(&[byte]).expect("mutate bundle member");
            file.sync_data().expect("sync bundle member mutation");
            byte ^= 1;
        }
    });
    thread::sleep(Duration::from_millis(10));
    let changed = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-25-bundle-race-publish")
        .json(&serde_json::json!({
            "source": {"path": bundle, "callerWorkingDirectory": harness._root.path()},
            "projectId": project["result"]["id"],
            "entry": "00-index.html",
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("source-changed directory Publish");
    mutating.store(false, Ordering::Release);
    writer.join().expect("bundle mutation writer");
    assert_eq!(changed.status(), 422);
    assert_eq!(
        changed.json::<Value>().expect("bundle source_changed JSON")["error"]["code"],
        "source_changed"
    );
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("bundle race catalogue");
    for table in ["artifacts", "revisions"] {
        let count: u64 = catalogue
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("bundle race visible count");
        assert_eq!(count, 0, "table={table}");
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn directory_publish_detects_added_member_after_intent_acceptance() {
    let harness = Harness::start_configured(
        |_| {},
        |command| {
            command.env("OBS_TEST_HOLD_PUBLISH_AFTER_INTENT_MS", "400");
        },
    );
    let project_directory = harness._root.path().join("bundle-addition-project");
    fs::create_dir(&project_directory).expect("bundle addition Project");
    let bundle = harness._root.path().join("bundle-addition");
    fs::create_dir(&bundle).expect("bundle addition source");
    fs::write(bundle.join("index.html"), "<h1>Original tree</h1>").expect("bundle addition entry");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-addition-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Bundle Addition"}))
        .send()
        .expect("register bundle addition Project")
        .json::<Value>()
        .expect("bundle addition Project JSON");
    let publish_client = client.clone();
    let publish_url = harness.url("/api/v1/artifacts");
    let publish_root = harness._root.path().to_path_buf();
    let publish_bundle = bundle.clone();
    let project_id = project["result"]["id"].clone();
    let publish = thread::spawn(move || {
        publish_client
            .post(publish_url)
            .header("Idempotency-Key", "issue-25-addition-publish")
            .json(&serde_json::json!({
                "source": {"path": publish_bundle, "callerWorkingDirectory": publish_root},
                "projectId": project_id,
                "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
            }))
            .send()
            .expect("bundle addition Publish")
    });
    let mut accepted = false;
    for _ in 0..100 {
        let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
            .expect("bundle addition catalogue");
        let count: u64 = catalogue
            .query_row(
                "SELECT count(*) FROM operation_intents WHERE kind='artifact_publish'",
                [],
                |row| row.get(0),
            )
            .expect("bundle addition intent count");
        if count == 1 {
            accepted = true;
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(accepted, "Publish intent was not observed");
    fs::write(bundle.join("added.txt"), "added after acceptance").expect("add member after intent");
    let changed = publish.join().expect("bundle addition worker");
    assert_eq!(changed.status(), 422);
    assert_eq!(
        changed.json::<Value>().expect("bundle addition JSON")["error"]["code"],
        "source_changed"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn directory_publish_detects_removal_rename_metadata_and_link_count_races() {
    for mutation in ["remove", "rename", "metadata", "hardlink"] {
        assert_directory_tree_race(mutation);
    }
}

#[cfg(feature = "test-faults")]
fn assert_directory_tree_race(mutation: &str) {
    let harness = Harness::start_configured(
        |_| {},
        |command| {
            command.env("OBS_TEST_HOLD_PUBLISH_AFTER_INTENT_MS", "300");
        },
    );
    let project_directory = harness
        ._root
        .path()
        .join(format!("bundle-{mutation}-project"));
    fs::create_dir(&project_directory).expect("bundle tree-race Project");
    let bundle = harness._root.path().join(format!("bundle-{mutation}"));
    fs::create_dir(&bundle).expect("bundle tree-race source");
    fs::write(bundle.join("index.html"), "<h1>Tree race</h1>").expect("tree-race entry");
    fs::write(bundle.join("support.txt"), "support").expect("tree-race support");
    fs::write(
        bundle.join(".obs.json"),
        r#"{"schemaVersion":1,"entry":"index.html"}"#,
    )
    .expect("tree-race metadata");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", format!("issue-25-{mutation}-project"))
        .json(&serde_json::json!({"path": project_directory, "title": mutation}))
        .send()
        .expect("register tree-race Project")
        .json::<Value>()
        .expect("tree-race Project JSON");
    let publish_client = client.clone();
    let publish_url = harness.url("/api/v1/artifacts");
    let publish_bundle = bundle.clone();
    let caller = harness._root.path().to_path_buf();
    let project_id = project["result"]["id"].clone();
    let key = format!("issue-25-{mutation}-publish");
    let publish = thread::spawn(move || {
        publish_client
            .post(publish_url)
            .header("Idempotency-Key", key)
            .json(&serde_json::json!({
                "source": {"path": publish_bundle, "callerWorkingDirectory": caller},
                "projectId": project_id,
                "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
            }))
            .send()
            .expect("tree-race Publish")
    });
    for _ in 0..100 {
        let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
            .expect("tree-race catalogue");
        let accepted: bool = catalogue
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operation_intents WHERE kind='artifact_publish')",
                [],
                |row| row.get(0),
            )
            .expect("tree-race intent state");
        if accepted {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    match mutation {
        "remove" => fs::remove_file(bundle.join("support.txt")).expect("remove bundle member"),
        "rename" => fs::rename(bundle.join("support.txt"), bundle.join("renamed.txt"))
            .expect("rename bundle member"),
        "metadata" => fs::write(
            bundle.join(".obs.json"),
            r#"{"schemaVersion":1,"entry":"index.html","title":"changed"}"#,
        )
        .expect("change portable metadata"),
        "hardlink" => fs::hard_link(bundle.join("support.txt"), bundle.join("linked.txt"))
            .expect("change bundle link count"),
        _ => unreachable!(),
    }
    let changed = publish.join().expect("tree-race worker");
    assert_eq!(changed.status(), 422, "mutation={mutation}");
    assert_eq!(
        changed.json::<Value>().expect("tree-race error JSON")["error"]["code"],
        "source_changed",
        "mutation={mutation}"
    );
}

#[test]
fn artifact_replacement_preserves_identity_and_immutable_revision_history() {
    let harness = Harness::start_configured(
        |_| {},
        |command| {
            command.arg("--max-live-artifacts").arg("1");
        },
    );
    let project_directory = harness._root.path().join("replacement-project");
    fs::create_dir(&project_directory).expect("replacement Project");
    let first_source = harness._root.path().join("first.html");
    fs::write(&first_source, "<h1>First Revision</h1>").expect("first Revision source");
    let replacement = harness._root.path().join("replacement-bundle");
    fs::create_dir(&replacement).expect("replacement bundle");
    fs::write(replacement.join("index.html"), "<h1>Second Revision</h1>")
        .expect("replacement entry");
    fs::write(replacement.join("support.txt"), "supporting bytes").expect("replacement support");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-replacement-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Replacement Project"}))
        .send()
        .expect("register replacement Project")
        .json::<Value>()
        .expect("replacement Project JSON");
    let first = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-25-first-publish")
        .json(&serde_json::json!({
            "source": {"path": first_source, "callerWorkingDirectory": harness._root.path()},
            "projectId": project["result"]["id"],
            "title": "Stable Artifact",
            "retention": {"mode": "pinned", "ttlMs": null, "pinReason": "keep identity"}
        }))
        .send()
        .expect("first Publish")
        .json::<Value>()
        .expect("first Publish JSON");
    let artifact_id = first["result"]["artifact"]["id"]
        .as_str()
        .expect("Artifact ID");
    let artifact_key = first["result"]["artifact"]["key"]
        .as_str()
        .expect("Artifact key");
    let first_revision_id = first["result"]["revision"]["id"]
        .as_str()
        .expect("first Revision ID");
    let replace_path = format!("/api/v1/artifacts/{artifact_id}/replace");
    let missing_precondition = client
        .post(harness.url(&replace_path))
        .header("Idempotency-Key", "issue-25-missing-replace-precondition")
        .json(&serde_json::json!({
            "source": {"path": replacement, "callerWorkingDirectory": harness._root.path()}
        }))
        .send()
        .expect("replacement without If-Match");
    assert_eq!(missing_precondition.status(), 428);
    let stale = client
        .post(harness.url(&replace_path))
        .header("If-Match", "\"rv-99\"")
        .header("Idempotency-Key", "issue-25-stale-replacement")
        .json(&serde_json::json!({
            "source": {"path": replacement, "callerWorkingDirectory": harness._root.path()}
        }))
        .send()
        .expect("stale replacement");
    assert_eq!(stale.status(), 412);
    let failed = client
        .post(harness.url(&replace_path))
        .header("If-Match", "\"rv-1\"")
        .header("Idempotency-Key", "issue-25-failed-replacement")
        .json(&serde_json::json!({
            "source": {"path": harness._root.path().join("missing"), "callerWorkingDirectory": harness._root.path()}
        }))
        .send()
        .expect("failed replacement");
    assert!(failed.status().is_client_error() || failed.status().is_server_error());
    let replaced = client
        .post(harness.url(&replace_path))
        .header("If-Match", "\"rv-1\"")
        .header("Idempotency-Key", "issue-25-successful-replacement")
        .json(&serde_json::json!({
            "source": {"path": replacement, "callerWorkingDirectory": harness._root.path()},
            "title": "Stable Artifact",
            "slug": "advanced-stable"
        }))
        .send()
        .expect("successful replacement");
    let replacement_status = replaced.status();
    let replacement_etag = replaced.headers().get("etag").cloned();
    let replaced = replaced.json::<Value>().expect("replacement JSON");
    assert_eq!(replacement_status, 200, "{replaced:#}");
    assert_eq!(replacement_etag.expect("replacement ETag"), "\"rv-2\"");
    assert_eq!(replaced["result"]["artifact"]["id"], artifact_id);
    assert_eq!(
        replaced["result"]["artifact"]["key"],
        format!("advanced-stable~{artifact_id}")
    );
    assert_ne!(
        artifact_key,
        replaced["result"]["artifact"]["key"]
            .as_str()
            .expect("replacement key")
    );
    assert_eq!(replaced["result"]["artifact"]["recordVersion"], 2);
    assert_eq!(replaced["result"]["artifact"]["revisionCount"], 2);
    assert_eq!(
        replaced["result"]["artifact"]["retention"]["mode"],
        "pinned"
    );
    let second_revision_id = replaced["result"]["revision"]["id"]
        .as_str()
        .expect("second Revision ID");
    assert_ne!(second_revision_id, first_revision_id);
    let replay = client
        .post(harness.url(&replace_path))
        .header("If-Match", "\"rv-1\"")
        .header("Idempotency-Key", "issue-25-successful-replacement")
        .json(&serde_json::json!({
            "source": {"path": replacement, "callerWorkingDirectory": harness._root.path()},
            "title": "Stable Artifact",
            "slug": "advanced-stable"
        }))
        .send()
        .expect("replacement replay");
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(
        replay.json::<Value>().expect("replacement replay JSON")["result"]["revision"]["id"],
        second_revision_id
    );
    let stable_path = url::Url::parse(
        replaced["result"]["artifact"]["openUrl"]
            .as_str()
            .expect("stable Open URL"),
    )
    .expect("stable Open URL parse")
    .path()
    .to_owned();
    let durable_slug: String = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("replacement slug catalogue")
        .query_row(
            "SELECT slug FROM artifacts WHERE id=?1",
            [artifact_id],
            |row| row.get(0),
        )
        .expect("durable replacement slug");
    assert_eq!(durable_slug, "advanced-stable");
    let no_redirect = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("no-redirect replacement client");
    let stale_slug = no_redirect
        .get(harness.url(&format!("/artifacts/{artifact_key}/")))
        .send()
        .expect("stale replacement slug redirect");
    assert_eq!(
        stale_slug.status(),
        308,
        "location={:?}",
        stale_slug.headers().get("location")
    );
    assert_eq!(
        stale_slug.headers()["location"],
        replaced["result"]["artifact"]["openUrl"]
            .as_str()
            .expect("replacement Open URL")
    );
    assert_eq!(
        client
            .get(harness.url(&stable_path))
            .send()
            .expect("stable replacement bytes")
            .text()
            .expect("stable replacement text"),
        "<h1>Second Revision</h1>"
    );
    assert_eq!(
        client
            .get(harness.url(&format!("/revisions/{first_revision_id}/")))
            .send()
            .expect("old immutable Revision")
            .text()
            .expect("old immutable bytes"),
        "<h1>First Revision</h1>"
    );
    let history = client
        .get(harness.url(&format!("/api/v1/artifacts/{artifact_id}/revisions")))
        .send()
        .expect("replacement Revision history")
        .json::<Value>()
        .expect("replacement history JSON");
    assert_eq!(
        history["result"]["items"]
            .as_array()
            .expect("history items")
            .len(),
        2
    );
    assert_eq!(history["result"]["items"][0]["id"], second_revision_id);
    assert_eq!(history["result"]["items"][0]["state"], "current");
    assert_eq!(history["result"]["items"][1]["id"], first_revision_id);
    assert_eq!(history["result"]["items"][1]["state"], "superseded");
    let first_page = client
        .get(harness.url(&format!("/api/v1/artifacts/{artifact_id}/revisions")))
        .query(&[
            ("availability", "all"),
            ("order", "published"),
            ("direction", "desc"),
            ("limit", "1"),
        ])
        .send()
        .expect("first Revision history page");
    assert_eq!(first_page.status(), 200);
    let next_link = first_page.headers()["link"]
        .to_str()
        .expect("Revision next Link")
        .strip_prefix('<')
        .and_then(|value| value.split_once('>'))
        .map(|(target, relation)| {
            assert_eq!(relation, "; rel=\"next\"");
            target
        })
        .expect("Revision RFC 8288 Link");
    let next_link = url::Url::parse(next_link).expect("absolute Revision next URL");
    let next_local = format!(
        "{}?{}",
        next_link.path(),
        next_link.query().expect("Revision continuation query")
    );
    let second_page = client
        .get(harness.url(&next_local))
        .send()
        .expect("second Revision history page")
        .json::<Value>()
        .expect("second Revision history JSON");
    assert_eq!(second_page["result"]["items"][0]["id"], first_revision_id);
    let superseded = client
        .get(harness.url(&format!("/api/v1/artifacts/{artifact_id}/revisions")))
        .query(&[
            ("availability", "superseded"),
            ("order", "superseded"),
            ("direction", "desc"),
        ])
        .send()
        .expect("superseded Revision history")
        .json::<Value>()
        .expect("superseded Revision history JSON");
    assert_eq!(superseded["result"]["items"][0]["id"], first_revision_id);
    let detail_path = url::Url::parse(
        replaced["result"]["artifact"]["detailUrl"]
            .as_str()
            .expect("replacement detail URL"),
    )
    .expect("replacement detail URL parse")
    .path()
    .to_owned();
    let detail = client
        .get(harness.url(&detail_path))
        .send()
        .expect("replacement browser detail")
        .text()
        .expect("replacement browser HTML");
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("replacement audit catalogue");
    let replacement_audits: u64 = catalogue
        .query_row(
            "SELECT count(*) FROM audit_events
             WHERE kind='artifact_replaced' AND resource_id=?1",
            [artifact_id],
            |row| row.get(0),
        )
        .expect("replacement audit count");
    assert_eq!(replacement_audits, 1);
    for expected in [
        "Revision history",
        first_revision_id,
        second_revision_id,
        "Revision · current",
        "Revision · superseded",
        "2 files",
    ] {
        assert!(
            detail.contains(expected),
            "missing {expected:?} in {detail}"
        );
    }
}

#[cfg(feature = "test-faults")]
#[test]
fn interrupted_replacement_preserves_old_current_then_recovers_once() {
    for fault in [
        "OBS_TEST_FAIL_PUBLISH_AFTER_INTENT",
        "OBS_TEST_FAIL_PUBLISH_AFTER_STAGE_SYNC",
        "OBS_TEST_FAIL_PUBLISH_AFTER_STAGED",
        "OBS_TEST_FAIL_PUBLISH_AFTER_FINALIZE",
        "OBS_TEST_FAIL_PUBLISH_AFTER_RENAME",
    ] {
        assert_interrupted_replacement_recovers_once(fault);
    }
}

#[cfg(feature = "test-faults")]
fn assert_interrupted_replacement_recovers_once(fault: &str) {
    let mut harness = Harness::start();
    let project_directory = harness._root.path().join("replacement-recovery-project");
    fs::create_dir(&project_directory).expect("replacement recovery Project");
    let first_source = harness._root.path().join("replacement-old.html");
    let next_source = harness._root.path().join("replacement-new-bundle");
    fs::write(&first_source, "<h1>Old current</h1>").expect("old replacement source");
    fs::create_dir(&next_source).expect("new replacement bundle");
    fs::write(next_source.join("index.html"), "<h1>Recovered current</h1>")
        .expect("new replacement entry");
    fs::write(next_source.join("support.txt"), "recovered support")
        .expect("new replacement support");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-recovery-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Replacement Recovery"}))
        .send()
        .expect("register replacement recovery Project")
        .json::<Value>()
        .expect("replacement recovery Project JSON");
    let first = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-25-recovery-first")
        .json(&serde_json::json!({
            "source": {"path": first_source, "callerWorkingDirectory": harness._root.path()},
            "projectId": project["result"]["id"],
            "title": "Recoverable Replacement",
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("Publish replacement recovery base")
        .json::<Value>()
        .expect("replacement recovery base JSON");
    let artifact_id = first["result"]["artifact"]["id"]
        .as_str()
        .expect("replacement recovery Artifact ID");
    let artifact_key = first["result"]["artifact"]["key"]
        .as_str()
        .expect("replacement recovery key");
    let old_revision_id = first["result"]["revision"]["id"]
        .as_str()
        .expect("old recovery Revision ID");
    let body = serde_json::json!({
        "source": {"path": next_source, "callerWorkingDirectory": harness._root.path()},
        "title": "Recoverable Replacement"
    });
    harness.restart_configured(|command| {
        command.env(fault, "1");
    });
    let interrupted = client
        .post(harness.url(&format!("/api/v1/artifacts/{artifact_id}/replace")))
        .header("If-Match", "\"rv-1\"")
        .header("Idempotency-Key", "issue-25-recovery-replace")
        .json(&body)
        .send()
        .expect("interrupt replacement after rename");
    assert_eq!(interrupted.status(), 500);
    let stable_path = format!("/artifacts/{artifact_key}/");
    assert_eq!(
        client
            .get(harness.url(&stable_path))
            .send()
            .expect("old current after interrupted replacement")
            .text()
            .expect("old current bytes"),
        "<h1>Old current</h1>"
    );
    harness.restart_configured(|_| {});
    let replay = client
        .post(harness.url(&format!("/api/v1/artifacts/{artifact_id}/replace")))
        .header("If-Match", "\"rv-1\"")
        .header("Idempotency-Key", "issue-25-recovery-replace")
        .json(&body)
        .send()
        .expect("replay recovered replacement");
    assert_eq!(replay.status(), 200);
    if fault != "OBS_TEST_FAIL_PUBLISH_AFTER_INTENT" {
        assert_eq!(replay.headers()["idempotency-replayed"], "true");
    }
    let replay = replay.json::<Value>().expect("recovered replacement JSON");
    assert_eq!(replay["result"]["artifact"]["recordVersion"], 2);
    assert_eq!(replay["result"]["artifact"]["revisionCount"], 2);
    assert_ne!(replay["result"]["revision"]["id"], old_revision_id);
    assert_eq!(
        client
            .get(harness.url(&stable_path))
            .send()
            .expect("recovered current bytes")
            .text()
            .expect("recovered current text"),
        "<h1>Recovered current</h1>"
    );
}

#[test]
fn replacement_visibility_failure_rolls_back_selection_and_restart_commits_once() {
    let mut harness = Harness::start();
    let project_directory = harness._root.path().join("replacement-rollback-project");
    fs::create_dir(&project_directory).expect("replacement rollback Project");
    let old_source = harness._root.path().join("rollback-old.html");
    let new_source = harness._root.path().join("rollback-new.html");
    fs::write(&old_source, "<p>old selection</p>").expect("old rollback source");
    fs::write(&new_source, "<p>new selection</p>").expect("new rollback source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-25-replace-rollback-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Replace Rollback"}))
        .send()
        .expect("register replacement rollback Project")
        .json::<Value>()
        .expect("replacement rollback Project JSON");
    let first = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-25-replace-rollback-first")
        .json(&serde_json::json!({
            "source": {"path": old_source, "callerWorkingDirectory": harness._root.path()},
            "projectId": project["result"]["id"],
            "title": "Rollback Artifact",
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("Publish replacement rollback base")
        .json::<Value>()
        .expect("replacement rollback base JSON");
    let artifact_id = first["result"]["artifact"]["id"]
        .as_str()
        .expect("rollback Artifact ID");
    let old_revision_id = first["result"]["revision"]["id"]
        .as_str()
        .expect("rollback old Revision ID");
    let catalogue_path = harness.storage.join("catalogue.sqlite");
    let catalogue =
        rusqlite::Connection::open(&catalogue_path).expect("replacement rollback catalogue");
    catalogue
        .execute_batch(
            "CREATE TRIGGER inject_artifact_replace_audit_failure
             BEFORE INSERT ON audit_events
             WHEN NEW.kind='artifact_replaced'
             BEGIN SELECT RAISE(ABORT, 'injected replacement audit failure'); END;",
        )
        .expect("install replacement visibility fault");
    let body = serde_json::json!({
        "source": {"path": new_source, "callerWorkingDirectory": harness._root.path()},
        "title": "Rollback Artifact"
    });
    let failed = client
        .post(harness.url(&format!("/api/v1/artifacts/{artifact_id}/replace")))
        .header("If-Match", "\"rv-1\"")
        .header("Idempotency-Key", "issue-25-replace-rollback")
        .json(&body)
        .send()
        .expect("fault replacement visibility transaction");
    assert_eq!(failed.status(), 500);
    let authority: (u64, String, u64) = catalogue
        .query_row(
            "SELECT record_version,current_revision_id,(SELECT count(*) FROM revisions)
             FROM artifacts WHERE id=?1",
            [artifact_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("rolled-back replacement authority");
    assert_eq!(authority, (1, old_revision_id.to_owned(), 1));
    catalogue
        .execute_batch("DROP TRIGGER inject_artifact_replace_audit_failure;")
        .expect("remove replacement visibility fault");
    drop(catalogue);
    harness.restart_configured(|_| {});
    let replay = client
        .post(harness.url(&format!("/api/v1/artifacts/{artifact_id}/replace")))
        .header("If-Match", "\"rv-1\"")
        .header("Idempotency-Key", "issue-25-replace-rollback")
        .json(&body)
        .send()
        .expect("replay replacement visibility recovery");
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    let catalogue =
        rusqlite::Connection::open(catalogue_path).expect("replacement recovered catalogue");
    let authority: (u64, u64, u64) = catalogue
        .query_row(
            "SELECT record_version,revision_count,(SELECT count(*) FROM revisions)
             FROM artifacts WHERE id=?1",
            [artifact_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("recovered replacement authority");
    assert_eq!(authority, (2, 2, 2));
}

#[test]
fn published_artifact_is_discoverable_through_api_ledger_and_browser_details() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("artifact-discovery-project");
    fs::create_dir(&project_directory).expect("Artifact discovery Project directory");
    let source = harness._root.path().join("discovery.txt");
    fs::write(&source, "discoverable bytes").expect("discoverable Artifact source");
    let client = reqwest::blocking::Client::new();
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-discovery-project")
        .json(&serde_json::json!({
            "path": project_directory.to_str().expect("Project path"),
            "title": "Discovery Project",
            "slug": "discovery-project"
        }))
        .send()
        .expect("register discovery Project")
        .json::<Value>()
        .expect("discovery Project result");
    let project = &registered["result"];
    let project_id = project["id"].as_str().expect("Project ID");
    let project_key = project["key"].as_str().expect("Project key");
    let published = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-discovery-publish")
        .json(&serde_json::json!({
            "source": {
                "path": source.to_str().expect("source path"),
                "callerWorkingDirectory": harness._root.path().to_str().expect("caller cwd")
            },
            "projectId": project_id,
            "title": "Discovery Note",
            "description": "Visible from every read adapter",
            "slug": "discovery-note",
            "retention": {"mode": "pinned", "ttlMs": null, "pinReason": "test fixture"}
        }))
        .send()
        .expect("publish discoverable Artifact")
        .json::<Value>()
        .expect("discoverable Publish result");
    let artifact = published["result"]["artifact"].clone();
    let revision = published["result"]["revision"].clone();
    let artifact_id = artifact["id"].as_str().expect("Artifact ID");
    let revision_id = revision["id"].as_str().expect("Revision ID");

    let shown = client
        .get(harness.url(&format!("/api/v1/artifacts/{artifact_id}")))
        .send()
        .expect("show Artifact");
    assert_eq!(shown.status(), 200);
    assert_eq!(shown.headers()["etag"], "\"rv-1\"");
    assert_eq!(
        shown.json::<Value>().expect("shown Artifact")["result"],
        artifact
    );

    let shown_revision = client
        .get(harness.url(&format!("/api/v1/revisions/{revision_id}")))
        .send()
        .expect("show Revision");
    assert_eq!(shown_revision.status(), 200);
    assert_eq!(
        shown_revision.json::<Value>().expect("shown Revision")["result"],
        revision
    );

    let listed = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[("projectId", project_id), ("order", "recent")])
        .send()
        .expect("list Artifacts");
    assert_eq!(listed.status(), 200);
    let listed = listed.json::<Value>().expect("Artifact list");
    assert_eq!(
        listed["result"]["items"],
        serde_json::json!([artifact.clone()])
    );
    assert_eq!(listed["result"]["page"]["hasMore"], false);

    for path in [
        "/api/v1/projects/ledger".to_owned(),
        format!("/api/v1/projects/{project_id}/ledger"),
    ] {
        let ledger = client
            .get(harness.url(&path))
            .query(&[("projectId", project_id), ("kind", "artifact")])
            .send()
            .expect("Artifact ledger");
        assert_eq!(ledger.status(), 200);
        let ledger = ledger.json::<Value>().expect("Artifact ledger result");
        assert_eq!(
            ledger["result"]["items"],
            serde_json::json!([artifact.clone()])
        );
    }

    let project_detail = client
        .get(harness.url(&format!("/ui/projects/{project_key}/")))
        .send()
        .expect("Project detail with Artifact");
    assert_eq!(project_detail.status(), 200);
    let project_detail = project_detail.text().expect("Project detail HTML");
    assert!(project_detail.contains("Discovery Note"));
    assert!(project_detail.contains("ARTIFACT"));
    assert!(project_detail.contains("PINNED"));
    assert!(project_detail.contains(artifact["key"].as_str().expect("Artifact key")));
    assert!(project_detail.contains(project_key));
    assert!(project_detail.contains(revision_id));
    assert!(project_detail.contains("1 file · 18 logical bytes · 1 Revision"));
    assert!(project_detail.contains(artifact["publishedAt"].as_str().expect("Publish instant")));
    assert!(project_detail.contains(artifact["openUrl"].as_str().expect("Artifact Open URL")));
    assert!(project_detail.contains(artifact["detailUrl"].as_str().expect("Artifact detail URL")));

    let detail_path = url::Url::parse(artifact["detailUrl"].as_str().expect("detail URL"))
        .expect("canonical detail URL")
        .path()
        .to_owned();
    let artifact_detail = client
        .get(harness.url(&detail_path))
        .send()
        .expect("Artifact detail");
    assert_eq!(artifact_detail.status(), 200);
    let artifact_detail = artifact_detail.text().expect("Artifact detail HTML");
    assert!(artifact_detail.contains("Discovery Note"));
    assert!(artifact_detail.contains("Visible from every read adapter"));
    assert!(artifact_detail.contains("Retention"));
    assert!(artifact_detail.contains("PINNED"));
    assert!(artifact_detail.contains("Pin reason"));
    assert!(artifact_detail.contains("test fixture"));
    assert!(artifact_detail.contains(revision_id));
    assert!(artifact_detail.contains(revision["openUrl"].as_str().expect("Revision Open URL")));
}

#[test]
fn artifact_cli_publishes_lists_and_shows_through_the_daemon() {
    let harness = Harness::start();
    let server = format!("http://{}", harness.address);
    let project_directory = harness._root.path().join("artifact-cli-project");
    fs::create_dir(&project_directory).expect("Artifact CLI Project directory");
    let project_path = project_directory
        .to_str()
        .expect("Artifact CLI Project path");
    let registered = cli(
        &server,
        &[
            "--idempotency-key",
            "issue-24-artifact-cli-project",
            "project",
            "register",
            project_path,
            "--title",
            "Artifact CLI Project",
        ],
    );
    assert!(registered.status.success(), "{registered:?}");
    let source = harness._root.path().join("cli-note.md");
    fs::write(&source, "# CLI note\n\nPublished through obs.\n").expect("CLI source");

    let missing_key = cli(
        &server,
        &[
            "-p",
            project_path,
            "artifact",
            "publish",
            source.to_str().expect("CLI source path"),
        ],
    );
    assert_eq!(missing_key.status.code(), Some(2));
    assert!(missing_key.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&missing_key.stderr).expect("missing key JSON")["error"]["code"],
        "invalid_idempotency_key"
    );

    let published = cli(
        &server,
        &[
            "-p",
            project_path,
            "--idempotency-key",
            "issue-24-artifact-cli-publish",
            "artifact",
            "publish",
            source.to_str().expect("CLI source path"),
            "--title",
            "CLI Note",
            "--description",
            "Agent-facing Publish",
            "--pin",
            "--reason",
            "CLI fixture",
        ],
    );
    assert!(published.status.success(), "{published:?}");
    assert!(published.stderr.is_empty());
    let published: Value = serde_json::from_slice(&published.stdout).expect("CLI Publish JSON");
    let artifact = &published["result"]["artifact"];
    let artifact_id = artifact["id"].as_str().expect("CLI Artifact ID");
    let artifact_key = artifact["key"].as_str().expect("CLI Artifact key");
    assert_eq!(artifact["title"], "CLI Note");
    assert_eq!(artifact["retention"]["mode"], "pinned");

    let replayed = cli(
        &server,
        &[
            "-p",
            project_path,
            "--idempotency-key",
            "issue-24-artifact-cli-publish",
            "artifact",
            "publish",
            source.to_str().expect("CLI source path"),
            "--title",
            "CLI Note",
            "--description",
            "Agent-facing Publish",
            "--pin",
            "--reason",
            "CLI fixture",
        ],
    );
    assert!(replayed.status.success(), "{replayed:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&replayed.stdout).expect("CLI replay JSON")["result"]["artifact"]
            ["id"],
        artifact_id
    );

    let zulu_source = harness._root.path().join("zulu-note.md");
    fs::write(&zulu_source, "# Zulu note\n").expect("Zulu CLI source");
    let zulu = cli(
        &server,
        &[
            "-p",
            project_path,
            "--idempotency-key",
            "issue-24-artifact-cli-zulu",
            "artifact",
            "publish",
            zulu_source.to_str().expect("Zulu CLI source path"),
            "--title",
            "Zulu Note",
        ],
    );
    assert!(zulu.status.success(), "{zulu:?}");
    let selection_directory = harness._root.path().join("selection-directory");
    fs::create_dir(&selection_directory).expect("relative selection directory");
    let relative_source = harness._root.path().join("relative-note.html");
    fs::write(&relative_source, "<p>relative</p>").expect("relative CLI source");
    let relative = obs()
        .current_dir(&selection_directory)
        .args([
            "--json",
            "--server",
            &server,
            "-p",
            project_path,
            "--idempotency-key",
            "issue-24-artifact-cli-relative",
            "artifact",
            "publish",
            "../relative-note.html",
            "--title",
            "Middle Note",
        ])
        .output()
        .expect("relative Artifact Publish CLI");
    assert!(relative.status.success(), "{relative:?}");

    let listed = cli(
        &server,
        &["-p", project_path, "artifact", "list", "--order", "title"],
    );
    assert!(listed.status.success(), "{listed:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&listed.stdout).expect("CLI list JSON")["result"]["items"]
            [0]["title"],
        "Zulu Note"
    );
    let list_help = obs()
        .args(["artifact", "list", "--help"])
        .output()
        .expect("Artifact list help");
    assert!(list_help.status.success());
    assert!(!String::from_utf8_lossy(&list_help.stdout).contains("--direction"));

    for selector in [artifact_id, artifact_key] {
        let shown = cli(&server, &["artifact", "show", selector]);
        assert!(shown.status.success(), "selector={selector}: {shown:?}");
        assert_eq!(
            serde_json::from_slice::<Value>(&shown.stdout).expect("CLI show JSON")["result"]["id"],
            artifact_id
        );
    }
    let revisions = cli(&server, &["artifact", "show", artifact_id, "--revisions"]);
    assert!(revisions.status.success(), "{revisions:?}");
    let revisions: Value =
        serde_json::from_slice(&revisions.stdout).expect("CLI Revision history JSON");
    assert_eq!(
        revisions["result"]["items"][0]["id"],
        published["result"]["revision"]["id"]
    );
    let all = cli(
        &server,
        &["artifact", "list", "--all", "--retention", "pinned"],
    );
    assert!(all.status.success(), "{all:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&all.stdout).expect("all Artifact list JSON")["result"]["items"]
            [0]["id"],
        artifact_id
    );
    let invalid_retention = cli(
        &server,
        &["artifact", "list", "--all", "--retention", "typo"],
    );
    assert_eq!(invalid_retention.status.code(), Some(2));

    let replacement_source = harness._root.path().join("cli-replacement.html");
    fs::write(&replacement_source, "<h1>CLI replacement</h1>").expect("CLI replacement source");
    let replaced = cli(
        &server,
        &[
            "--idempotency-key",
            "issue-25-cli-replacement",
            "artifact",
            "replace",
            artifact_key,
            replacement_source
                .to_str()
                .expect("CLI replacement source path"),
            "--title",
            "CLI Note Replaced",
        ],
    );
    assert!(replaced.status.success(), "{replaced:?}");
    let replaced: Value = serde_json::from_slice(&replaced.stdout).expect("CLI replacement JSON");
    assert_eq!(replaced["result"]["artifact"]["id"], artifact_id);
    assert_eq!(replaced["result"]["artifact"]["recordVersion"], 2);
    assert_eq!(
        replaced["result"]["artifact"]["retention"]["mode"],
        "pinned"
    );
    let history = cli(&server, &["artifact", "show", artifact_id, "--revisions"]);
    assert!(history.status.success(), "{history:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&history.stdout).expect("CLI replaced history JSON")
            ["result"]["items"]
            .as_array()
            .expect("CLI replaced history items")
            .len(),
        2
    );
}

#[test]
fn artifact_publish_rejects_unsafe_or_unsupported_sources_without_path_disclosure() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("artifact-safety-project");
    fs::create_dir(&project_directory).expect("Artifact safety Project");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-artifact-safety-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Artifact Safety"}))
        .send()
        .expect("register safety Project")
        .json::<Value>()
        .expect("safety Project result");
    let project_id = project["result"]["id"].as_str().expect("Project ID");
    let source_directory = harness._root.path().join("private-home-component");
    fs::create_dir(&source_directory).expect("private source directory");
    let unsupported = source_directory.join("payload.zip");
    fs::write(&unsupported, b"not-an-entry").expect("unsupported source");
    let regular = source_directory.join("regular.html");
    fs::write(
        &regular,
        b"<!doctype html><title>Derived title</title><p>safe</p>",
    )
    .expect("regular source");
    let symlink = source_directory.join("linked.html");
    std::os::unix::fs::symlink(&regular, &symlink).expect("source symlink");
    let hard_link = source_directory.join("hard-linked.html");
    fs::hard_link(&regular, &hard_link).expect("source hard link");
    let backslash = source_directory.join("unsafe\\name.html");
    fs::write(&backslash, "unsafe route name").expect("backslash source");
    let double_decode = source_directory.join("unsafe%2Fname.html");
    fs::write(&double_decode, "double-decode route name").expect("double-decode source");

    for (ordinal, source, expected_status, expected_code) in [
        ("unsupported", &unsupported, 415, "unsupported_entry_media"),
        ("symlink", &symlink, 422, "invalid_input"),
        ("hardlink", &hard_link, 422, "unsafe_source"),
        ("backslash", &backslash, 422, "invalid_source"),
        ("double-decode", &double_decode, 422, "invalid_source"),
    ] {
        let response = client
            .post(harness.url("/api/v1/artifacts"))
            .header("Idempotency-Key", format!("issue-24-unsafe-{ordinal}"))
            .json(&serde_json::json!({
                "source": {
                    "path": source,
                    "callerWorkingDirectory": harness._root.path()
                },
                "projectId": project_id,
                "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
            }))
            .send()
            .expect("rejected Publish");
        assert_eq!(response.status(), expected_status, "case={ordinal}");
        let body = response.text().expect("rejected Publish body");
        assert_eq!(
            serde_json::from_str::<Value>(&body).expect("rejected Publish JSON")["error"]["code"],
            expected_code
        );
        assert!(!body.contains("private-home-component"), "{body}");
        assert!(
            !body.contains(harness._root.path().to_str().expect("root path")),
            "{body}"
        );
    }

    fs::remove_file(&hard_link).expect("remove extra hard link");
    let published = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-derived-title")
        .json(&serde_json::json!({
            "source": {"path": regular, "callerWorkingDirectory": harness._root.path()},
            "projectId": project_id,
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("Publish with derived title");
    assert_eq!(published.status(), 201);
    let published = published.json::<Value>().expect("derived title Publish");
    assert_eq!(published["result"]["artifact"]["title"], "Derived title");

    let conflict = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-derived-title")
        .json(&serde_json::json!({
            "source": {"path": regular, "callerWorkingDirectory": harness._root.path()},
            "projectId": project_id,
            "title": "Changed request",
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("Publish fingerprint conflict");
    assert_eq!(conflict.status(), 409);
    assert_eq!(
        conflict.json::<Value>().expect("fingerprint conflict")["error"]["code"],
        "idempotency_conflict"
    );
}

#[test]
fn artifact_publish_capacity_failure_precedes_storage_and_visibility() {
    let harness = Harness::start_configured(
        |_| {},
        |command| {
            command.args(["--max-stored-bytes", "4", "--max-live-artifacts", "1"]);
        },
    );
    let project_directory = harness._root.path().join("capacity-project");
    fs::create_dir(&project_directory).expect("capacity Project");
    let source = harness._root.path().join("too-large.txt");
    fs::write(&source, "five!").expect("capacity source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-capacity-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Capacity"}))
        .send()
        .expect("register capacity Project")
        .json::<Value>()
        .expect("capacity Project result");
    let project_id = project["result"]["id"].as_str().expect("Project ID");
    let blocked = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-capacity-publish")
        .json(&serde_json::json!({
            "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
            "projectId": project_id,
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("capacity-blocked Publish");
    assert_eq!(blocked.status(), 507);
    let blocked = blocked.json::<Value>().expect("capacity error");
    assert_eq!(blocked["error"]["code"], "capacity");
    assert_eq!(
        blocked["error"]["details"]["blockingConstraint"],
        "max_stored_bytes"
    );
    assert_eq!(blocked["error"]["details"]["requiredBytes"], 5);
    for field in [
        "requiredBytes",
        "accountedStoredBytes",
        "maxStoredBytes",
        "liveArtifacts",
        "maxLiveArtifacts",
        "filesystemAvailableBytes",
        "reserveBytes",
        "reclaimableBytes",
        "blockingConstraint",
    ] {
        assert!(
            blocked["error"]["details"].get(field).is_some(),
            "missing capacity field {field}"
        );
    }

    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("capacity catalogue");
    for table in ["artifacts", "revisions", "operation_intents"] {
        let count: u64 = catalogue
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("capacity table count");
        assert_eq!(count, 0, "table={table}");
    }
    assert!(
        fs::read_dir(harness.storage.join("staging"))
            .expect("staging")
            .next()
            .is_none()
    );
    assert!(
        fs::read_dir(harness.storage.join("revisions"))
            .expect("revisions")
            .next()
            .is_none()
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn concurrent_publish_capacity_counts_the_first_durable_reservation() {
    let harness = Harness::start_configured(
        |_| {},
        |command| {
            command.args(["--max-stored-bytes", "5"]);
            command.env("OBS_TEST_HOLD_PUBLISH_AFTER_INTENT_MS", "400");
        },
    );
    let project_directory = harness._root.path().join("capacity-reservation-project");
    fs::create_dir(&project_directory).expect("capacity reservation Project");
    let source = harness._root.path().join("five.txt");
    fs::write(&source, "five!").expect("capacity reservation source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-capacity-reservation-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Reservations"}))
        .send()
        .expect("register capacity reservation Project")
        .json::<Value>()
        .expect("capacity reservation Project JSON");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project["result"]["id"],
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let first_client = client.clone();
    let publish_url = harness.url("/api/v1/artifacts");
    let first_url = publish_url.clone();
    let first_body = body.clone();
    let first = thread::spawn(move || {
        first_client
            .post(first_url)
            .header("Idempotency-Key", "issue-24-capacity-reservation-first")
            .json(&first_body)
            .send()
            .expect("first reserved Publish")
    });
    let mut reservation_observed = false;
    for _ in 0..100 {
        let connection = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
            .expect("capacity reservation catalogue");
        let count: u64 = connection
            .query_row(
                "SELECT count(*) FROM operation_intents WHERE kind='artifact_publish'",
                [],
                |row| row.get(0),
            )
            .expect("capacity reservation count");
        if count == 1 {
            reservation_observed = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        reservation_observed,
        "first Publish reservation was not observed"
    );
    let blocked = client
        .post(publish_url)
        .header("Idempotency-Key", "issue-24-capacity-reservation-second")
        .json(&body)
        .send()
        .expect("second capacity-blocked Publish");
    assert_eq!(blocked.status(), 507);
    let blocked = blocked.json::<Value>().expect("reserved capacity error");
    assert_eq!(blocked["error"]["details"]["accountedStoredBytes"], 5);
    assert_eq!(
        blocked["error"]["details"]["blockingConstraint"],
        "max_stored_bytes"
    );
    assert_eq!(first.join().expect("first Publish worker").status(), 201);
}

#[test]
fn source_change_binds_the_key_and_requires_a_new_key_for_later_bytes() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("source-race-project");
    fs::create_dir(&project_directory).expect("source race Project");
    let source = harness._root.path().join("changing.txt");
    let mut source_file = fs::File::create(&source).expect("source race file");
    source_file
        .set_len(32 * 1024 * 1024)
        .expect("size source race file");
    source_file
        .write_all(b"source race")
        .expect("initialize source race file");
    source_file.sync_all().expect("sync source race file");
    drop(source_file);
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-source-race-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Source Race"}))
        .send()
        .expect("register source race Project")
        .json::<Value>()
        .expect("source race Project result");
    let project_id = project["result"]["id"].as_str().expect("Project ID");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project_id,
        "title": "Changing Source",
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let mutating = Arc::new(AtomicBool::new(true));
    let writer_flag = mutating.clone();
    let writer_source = source.clone();
    let writer = thread::spawn(move || {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(writer_source)
            .expect("open changing source writer");
        let mut byte = 0_u8;
        while writer_flag.load(Ordering::Acquire) {
            file.seek(SeekFrom::End(-1)).expect("seek changing source");
            file.write_all(&[byte]).expect("mutate changing source");
            file.sync_data().expect("sync changing source mutation");
            byte ^= 1;
        }
    });
    thread::sleep(Duration::from_millis(10));
    let changed = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-source-race-publish")
        .json(&body)
        .send()
        .expect("source-changed Publish");
    mutating.store(false, Ordering::Release);
    writer.join().expect("source mutation writer");
    assert_eq!(changed.status(), 422);
    assert_eq!(
        changed.json::<Value>().expect("source_changed JSON")["error"]["code"],
        "source_changed"
    );
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("source race catalogue");
    for table in ["artifacts", "revisions"] {
        let count: u64 = catalogue
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("source race authority count");
        assert_eq!(count, 0, "table={table}");
    }
    let failed_state: String = catalogue
        .query_row(
            "SELECT state FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| row.get(0),
        )
        .expect("failed source race intent");
    assert_eq!(failed_state, "failed_terminal");
    drop(catalogue);

    let replay = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-source-race-publish")
        .json(&body)
        .send()
        .expect("source race replay");
    assert_eq!(replay.status(), 422);
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    assert_eq!(
        replay.json::<Value>().expect("source race replay JSON")["error"]["code"],
        "source_changed"
    );
    let later_bytes = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-source-race-later-bytes")
        .json(&body)
        .send()
        .expect("source race new request");
    assert_eq!(later_bytes.status(), 201);
    assert_eq!(
        later_bytes
            .json::<Value>()
            .expect("source race new request JSON")["result"]["artifact"]["title"],
        "Changing Source"
    );
}

#[cfg(feature = "test-faults")]
#[test]
fn interrupted_publish_reconciles_before_restart_readiness_and_replays_exactly() {
    let mut harness = Harness::start_configured(
        |_| {},
        |command| {
            command.env("OBS_TEST_FAIL_PUBLISH_AFTER_RENAME", "1");
        },
    );
    let project_directory = harness._root.path().join("publish-recovery-project");
    fs::create_dir(&project_directory).expect("Publish recovery Project");
    let source = harness._root.path().join("recover.html");
    fs::write(
        &source,
        "<!doctype html><title>Recovered</title><p>durable</p>",
    )
    .expect("Publish recovery source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-recovery-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Recovery"}))
        .send()
        .expect("register recovery Project")
        .json::<Value>()
        .expect("recovery Project result");
    let project_id = project["result"]["id"].as_str().expect("Project ID");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project_id,
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let interrupted = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-recovery-publish")
        .json(&body)
        .send()
        .expect("injected interrupted Publish");
    assert_eq!(interrupted.status(), 500);
    assert_eq!(
        interrupted.json::<Value>().expect("injected failure")["error"]["code"],
        "internal"
    );
    let before = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[("projectId", project_id)])
        .send()
        .expect("Artifact list before recovery")
        .json::<Value>()
        .expect("Artifact list before recovery JSON");
    assert_eq!(before["result"]["items"], serde_json::json!([]));
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("interrupted Publish catalogue");
    let state: String = catalogue
        .query_row(
            "SELECT state FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| row.get(0),
        )
        .expect("interrupted Publish state");
    assert_eq!(state, "renamed");
    assert_eq!(
        fs::read_dir(harness.storage.join("revisions"))
            .expect("durable Revisions")
            .count(),
        1
    );
    drop(catalogue);

    harness.restart_configured(|_| {});
    let replay = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-recovery-publish")
        .json(&body)
        .send()
        .expect("replay recovered Publish");
    assert_eq!(replay.status(), 201);
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    let replay = replay.json::<Value>().expect("recovered Publish result");
    let artifact = &replay["result"]["artifact"];
    let open_path = url::Url::parse(artifact["openUrl"].as_str().expect("recovered Open URL"))
        .expect("recovered Open URL parse")
        .path()
        .to_owned();
    let served = client
        .get(harness.url(&open_path))
        .send()
        .expect("recovered Artifact bytes");
    assert_eq!(served.status(), 200);
    assert_eq!(
        served.bytes().expect("recovered bytes").as_ref(),
        b"<!doctype html><title>Recovered</title><p>durable</p>"
    );
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("recovered Publish catalogue");
    for table in ["artifacts", "revisions"] {
        let count: u64 = catalogue
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("recovered row count");
        assert_eq!(count, 1, "table={table}");
    }
    let operation_state: String = catalogue
        .query_row(
            "SELECT state FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| row.get(0),
        )
        .expect("recovered operation state");
    assert_eq!(operation_state, "completed");
}

#[cfg(feature = "test-faults")]
#[test]
fn publish_restart_resumes_each_synced_previsibility_phase() {
    for (fault, suffix) in [
        ("OBS_TEST_FAIL_PUBLISH_AFTER_INTENT", "intent-recorded"),
        ("OBS_TEST_FAIL_PUBLISH_AFTER_STAGE_SYNC", "stage-sync"),
        ("OBS_TEST_FAIL_PUBLISH_AFTER_STAGED", "staged"),
        ("OBS_TEST_FAIL_PUBLISH_AFTER_FINALIZE", "finalize"),
    ] {
        assert_publish_recovers_from_phase(fault, suffix);
    }
}

#[cfg(feature = "test-faults")]
fn assert_publish_recovers_from_phase(fault: &str, suffix: &str) {
    let mut harness = Harness::start_configured(
        |_| {},
        |command| {
            command.env(fault, "1");
        },
    );
    let project_directory = harness._root.path().join(format!("phase-{suffix}-project"));
    fs::create_dir(&project_directory).expect("phase recovery Project");
    let source = harness._root.path().join(format!("phase-{suffix}.html"));
    fs::write(&source, format!("<!doctype html><title>{suffix}</title>"))
        .expect("phase recovery source");
    let client = reqwest::blocking::Client::new();
    let project_key = format!("issue-24-phase-{suffix}-project");
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", project_key)
        .json(&serde_json::json!({"path": project_directory, "title": suffix}))
        .send()
        .expect("register phase recovery Project")
        .json::<Value>()
        .expect("phase recovery Project JSON");
    let publish_key = format!("issue-24-phase-{suffix}-publish");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project["result"]["id"],
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let interrupted = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", &publish_key)
        .json(&body)
        .send()
        .expect("phase-interrupted Publish");
    assert_eq!(interrupted.status(), 500, "fault={fault}");
    let (intent_artifact_id, intent_revision_id): (String, String) =
        rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
            .expect("phase intent catalogue")
            .query_row(
                "SELECT json_extract(details_json,'$.artifact.id'),
                        json_extract(details_json,'$.revision.id')
         FROM operation_intents WHERE kind='artifact_publish'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("phase intent identities");
    harness.restart_configured(|_| {});
    let mut changed_body = body.clone();
    changed_body["title"] = Value::String("changed request".to_owned());
    let conflict = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", &publish_key)
        .json(&changed_body)
        .send()
        .expect("phase recovery fingerprint conflict");
    assert_eq!(conflict.status(), 409, "fault={fault}");
    assert_eq!(
        conflict.json::<Value>().expect("phase conflict JSON")["error"]["code"],
        "idempotency_conflict"
    );
    let replay = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", publish_key)
        .json(&body)
        .send()
        .expect("phase recovery replay");
    assert_eq!(replay.status(), 201, "fault={fault}");
    if fault != "OBS_TEST_FAIL_PUBLISH_AFTER_INTENT" {
        assert_eq!(replay.headers()["idempotency-replayed"], "true");
    }
    let replay = replay.json::<Value>().expect("phase recovery JSON");
    assert_eq!(replay["result"]["artifact"]["id"], intent_artifact_id);
    assert_eq!(replay["result"]["revision"]["id"], intent_revision_id);
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("phase recovery catalogue");
    let state: String = catalogue
        .query_row(
            "SELECT state FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| row.get(0),
        )
        .expect("phase recovery state");
    assert_eq!(state, "completed", "fault={fault}");
}

#[cfg(feature = "test-faults")]
#[test]
fn publish_restart_classifies_every_storage_durability_boundary() {
    for (fault, suffix, completes) in [
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_COPY_DIGEST",
            "copy-digest",
            false,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_PAYLOAD_SYNC",
            "payload-sync",
            false,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_MANIFEST_WRITE",
            "manifest-write",
            true,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_MANIFEST_SYNC",
            "manifest-sync",
            true,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_CONTENT_SYNC",
            "content-sync",
            true,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_OPERATION_SYNC",
            "operation-sync",
            true,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_STAGING_SYNC",
            "staging-sync",
            true,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_STORAGE_RENAME",
            "storage-rename",
            true,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_RENAME_STAGING_SYNC",
            "rename-staging-sync",
            true,
        ),
        (
            "OBS_TEST_CRASH_PUBLISH_AFTER_RENAME_REVISIONS_SYNC",
            "rename-revisions-sync",
            true,
        ),
    ] {
        assert_storage_boundary_recovery(fault, suffix, completes);
    }
}

#[cfg(feature = "test-faults")]
fn assert_storage_boundary_recovery(fault: &str, suffix: &str, completes: bool) {
    let mut harness = Harness::start_configured(
        |_| {},
        |command| {
            command.env(fault, "1");
        },
    );
    let project_directory = harness
        ._root
        .path()
        .join(format!("boundary-{suffix}-project"));
    fs::create_dir(&project_directory).expect("storage boundary Project");
    let source = harness._root.path().join(format!("boundary-{suffix}.html"));
    fs::write(&source, format!("<title>{suffix}</title>")).expect("storage boundary source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header(
            "Idempotency-Key",
            format!("issue-24-boundary-{suffix}-project"),
        )
        .json(&serde_json::json!({"path": project_directory, "title": suffix}))
        .send()
        .expect("register storage boundary Project")
        .json::<Value>()
        .expect("storage boundary Project JSON");
    let key = format!("issue-24-boundary-{suffix}-publish");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project["result"]["id"],
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let interrupted = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", &key)
        .json(&body)
        .send()
        .expect("storage-boundary Publish response");
    assert_eq!(interrupted.status(), 500, "fault={fault}");
    harness.restart_configured(|_| {});
    let classified = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", &key)
        .json(&body)
        .send()
        .expect("classified storage-boundary retry");
    assert_eq!(classified.status(), if completes { 201 } else { 500 });
    if !completes {
        assert_eq!(classified.headers()["idempotency-replayed"], "true");
    }
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("storage boundary catalogue");
    let (state, visible): (String, u64) = catalogue
        .query_row(
            "SELECT state,(SELECT count(*) FROM artifacts)
             FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("storage boundary classification");
    assert_eq!(
        state,
        if completes {
            "completed"
        } else {
            "failed_terminal"
        }
    );
    assert_eq!(visible, u64::from(completes));
}

#[cfg(feature = "test-faults")]
#[test]
fn intent_recorded_retry_rejects_changed_source_without_new_visibility() {
    let mut harness = Harness::start_configured(
        |_| {},
        |command| {
            command
                .arg("--max-stored-bytes")
                .arg("5")
                .env("OBS_TEST_FAIL_PUBLISH_AFTER_INTENT", "1");
        },
    );
    let project_directory = harness._root.path().join("intent-source-project");
    fs::create_dir(&project_directory).expect("intent source Project");
    let source = harness._root.path().join("intent-source.html");
    fs::write(&source, "12345").expect("accepted source snapshot");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-intent-source-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Intent Source"}))
        .send()
        .expect("register intent source Project")
        .json::<Value>()
        .expect("intent source Project JSON");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project["result"]["id"],
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let interrupted = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-intent-source-publish")
        .json(&body)
        .send()
        .expect("interrupt intent source Publish");
    assert_eq!(interrupted.status(), 500);
    harness.restart_configured(|command| {
        command.arg("--max-stored-bytes").arg("5");
    });
    fs::write(&source, "123456").expect("change accepted source");
    let changed = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-intent-source-publish")
        .json(&body)
        .send()
        .expect("retry changed intent source");
    assert_eq!(changed.status(), 422);
    assert_eq!(
        changed.json::<Value>().expect("changed intent source JSON")["error"]["code"],
        "source_changed"
    );
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("intent source catalogue");
    for table in ["artifacts", "revisions"] {
        let count: u64 = catalogue
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("intent source visible row count");
        assert_eq!(count, 0, "table={table}");
    }
    let state: String = catalogue
        .query_row(
            "SELECT state FROM idempotency_requests
             WHERE key='issue-24-intent-source-publish'",
            [],
            |row| row.get(0),
        )
        .expect("changed source idempotency state");
    assert_eq!(state, "failed_terminal");
    drop(catalogue);
    let fresh_source = harness._root.path().join("fresh-after-terminal.html");
    fs::write(&fresh_source, "abcde").expect("fresh source after terminal failure");
    let mut fresh_body = body;
    fresh_body["source"]["path"] = serde_json::json!(fresh_source);
    let fresh = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-fresh-after-terminal")
        .json(&fresh_body)
        .send()
        .expect("Publish after byte-less terminal failure");
    assert_eq!(fresh.status(), 201);
}

#[cfg(feature = "test-faults")]
#[test]
fn awaiting_retry_publish_keeps_capacity_reserved_across_restart() {
    let mut harness = Harness::start_configured(
        |_| {},
        |command| {
            command
                .arg("--max-stored-bytes")
                .arg("5")
                .env("OBS_TEST_FAIL_PUBLISH_AFTER_INTENT", "1");
        },
    );
    let project_directory = harness._root.path().join("reserved-intent-project");
    fs::create_dir(&project_directory).expect("reserved intent Project");
    let accepted_source = harness._root.path().join("accepted.html");
    let competing_source = harness._root.path().join("competing.html");
    fs::write(&accepted_source, "12345").expect("accepted reserved source");
    fs::write(&competing_source, "abcde").expect("competing reserved source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-reserved-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Reserved Intent"}))
        .send()
        .expect("register reserved intent Project")
        .json::<Value>()
        .expect("reserved intent Project JSON");
    let project_id = project["result"]["id"].clone();
    let accepted_body = serde_json::json!({
        "source": {"path": accepted_source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project_id,
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let interrupted = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-reserved-publish")
        .json(&accepted_body)
        .send()
        .expect("interrupt reserved Publish");
    assert_eq!(interrupted.status(), 500);
    harness.restart_configured(|command| {
        command.arg("--max-stored-bytes").arg("5");
    });
    let competing = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-competing-publish")
        .json(&serde_json::json!({
            "source": {"path": competing_source, "callerWorkingDirectory": harness._root.path()},
            "projectId": project_id,
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("competing reserved Publish");
    assert_eq!(competing.status(), 507);
    assert_eq!(
        competing.json::<Value>().expect("competing capacity JSON")["error"]["details"]["accountedStoredBytes"],
        5
    );
    let resumed = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-reserved-publish")
        .json(&accepted_body)
        .send()
        .expect("resume capacity-reserved Publish");
    assert_eq!(resumed.status(), 201);
}

#[cfg(feature = "test-faults")]
#[test]
fn malformed_interrupted_publish_is_quarantined_and_bound_to_its_key() {
    let mut harness = Harness::start_configured(
        |_| {},
        |command| {
            command.env("OBS_TEST_FAIL_PUBLISH_AFTER_RENAME", "1");
        },
    );
    let project_directory = harness._root.path().join("malformed-recovery-project");
    fs::create_dir(&project_directory).expect("malformed recovery Project");
    let source = harness._root.path().join("malformed.html");
    fs::write(&source, "<!doctype html><title>Retry Cleanly</title>")
        .expect("malformed recovery source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-malformed-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Malformed Recovery"}))
        .send()
        .expect("register malformed recovery Project")
        .json::<Value>()
        .expect("malformed recovery Project result");
    let project_id = project["result"]["id"].as_str().expect("Project ID");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project_id,
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let interrupted = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-malformed-publish")
        .json(&body)
        .send()
        .expect("interrupt malformed Publish");
    assert_eq!(interrupted.status(), 500);
    let revision_directory = fs::read_dir(harness.storage.join("revisions"))
        .expect("interrupted Revision directory")
        .next()
        .expect("interrupted Revision entry")
        .expect("interrupted Revision entry result")
        .path();
    fs::write(
        revision_directory.join("revision-manifest.json"),
        b"{\"tampered\":true}",
    )
    .expect("tamper interrupted manifest");

    harness.restart_configured(|_| {});
    let listed = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[("projectId", project_id)])
        .send()
        .expect("list after malformed recovery")
        .json::<Value>()
        .expect("list after malformed recovery JSON");
    assert_eq!(listed["result"]["items"], serde_json::json!([]));
    assert!(
        fs::read_dir(harness.storage.join("revisions"))
            .expect("revisions")
            .next()
            .is_none()
    );
    assert_eq!(
        fs::read_dir(harness.storage.join("quarantine"))
            .expect("quarantine")
            .count(),
        1
    );
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("malformed recovery catalogue");
    let state: String = catalogue
        .query_row(
            "SELECT state FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| row.get(0),
        )
        .expect("malformed recovery state");
    assert_eq!(state, "failed_terminal");
    let idempotency_state: String = catalogue
        .query_row(
            "SELECT state FROM idempotency_requests WHERE key='issue-24-malformed-publish'",
            [],
            |row| row.get(0),
        )
        .expect("malformed idempotency state");
    assert_eq!(idempotency_state, "failed_terminal");
    drop(catalogue);

    let replay = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-malformed-publish")
        .json(&body)
        .send()
        .expect("replay malformed Publish");
    assert_eq!(replay.status(), 500);
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    let retried = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-malformed-new-attempt")
        .json(&body)
        .send()
        .expect("retry malformed Publish with new key");
    assert_eq!(retried.status(), 201);
    let retried = retried
        .json::<Value>()
        .expect("retried malformed Publish JSON");
    assert_eq!(retried["result"]["artifact"]["title"], "Retry Cleanly");
}

#[test]
fn artifact_and_revision_routes_canonicalize_reject_and_never_follow_tampered_members() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("artifact-route-project");
    fs::create_dir(&project_directory).expect("Artifact route Project");
    let source = harness._root.path().join("route.html");
    fs::write(&source, "<!doctype html><title>Routes</title><p>inside</p>")
        .expect("Artifact route source");
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("no-redirect client");
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-route-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Routes"}))
        .send()
        .expect("register route Project")
        .json::<Value>()
        .expect("route Project result");
    let project_id = project["result"]["id"].as_str().expect("Project ID");
    let published = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-route-publish")
        .json(&serde_json::json!({
            "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
            "projectId": project_id,
            "slug": "canonical-route",
            "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
        }))
        .send()
        .expect("route Publish")
        .json::<Value>()
        .expect("route Publish result");
    let artifact = &published["result"]["artifact"];
    let revision = &published["result"]["revision"];
    let artifact_id = artifact["id"].as_str().expect("Artifact ID");
    let artifact_key = artifact["key"].as_str().expect("Artifact key");
    let revision_id = revision["id"].as_str().expect("Revision ID");
    let canonical_artifact = artifact["openUrl"].as_str().expect("Artifact Open URL");
    let canonical_revision = revision["openUrl"].as_str().expect("Revision Open URL");

    for (path, location) in [
        (
            format!("/artifacts/stale~{artifact_id}/"),
            canonical_artifact,
        ),
        (
            format!(
                "/artifacts/canonical-route~{}/",
                artifact_id.to_ascii_uppercase()
            ),
            canonical_artifact,
        ),
        (format!("/artifacts/{artifact_key}"), canonical_artifact),
        (
            format!("/revisions/{}/", revision_id.to_ascii_uppercase()),
            canonical_revision,
        ),
        (format!("/revisions/{revision_id}"), canonical_revision),
    ] {
        let response = client
            .get(harness.url(&path))
            .send()
            .expect("canonical redirect");
        assert_eq!(response.status(), 308, "path={path}");
        assert_eq!(response.headers()["location"], location, "path={path}");
    }

    let stale_with_query = client
        .get(harness.url(&format!("/artifacts/stale~{artifact_id}/?view=compact")))
        .send()
        .expect("query-preserving canonical redirect");
    assert_eq!(stale_with_query.status(), 308);
    assert_eq!(
        stale_with_query.headers()["location"],
        format!("{canonical_artifact}?view=compact")
    );
    let malformed_slug = client
        .get(harness.url(&format!("/artifacts/Not_Valid~{artifact_id}/")))
        .send()
        .expect("malformed Artifact slug");
    assert_eq!(malformed_slug.status(), 422);

    let impossible_id = "z0000000000000000000000000";
    for path in [
        "/api/v1/artifacts/not-an-id".to_owned(),
        "/api/v1/revisions/not-an-id".to_owned(),
        format!("/api/v1/artifacts/{impossible_id}"),
        format!("/api/v1/revisions/{impossible_id}"),
        format!("/artifacts/impossible~{impossible_id}/"),
        format!("/revisions/{impossible_id}/"),
    ] {
        let malformed = client.get(harness.url(&path)).send().expect("malformed ID");
        assert_eq!(malformed.status(), 422, "path={path}");
    }
    let unknown_id = "70000000000000000000000000";
    for path in [
        format!("/api/v1/artifacts/{unknown_id}"),
        format!("/api/v1/revisions/{unknown_id}"),
        format!("/artifacts/unknown~{unknown_id}/"),
        format!("/revisions/{unknown_id}/"),
    ] {
        let unknown = client.get(harness.url(&path)).send().expect("unknown ID");
        assert_eq!(unknown.status(), 404, "path={path}");
    }
    let suffix = client
        .get(harness.url(&format!("/artifacts/{artifact_key}/missing")))
        .send()
        .expect("missing Artifact suffix");
    assert_eq!(suffix.status(), 404);
    let actual_member = client
        .get(harness.url(&format!("/artifacts/{artifact_key}/route.html?download=0")))
        .send()
        .expect("actual Artifact member");
    assert_eq!(actual_member.status(), 200);
    for suffix in ["%5Coutside", "%2Foutside", "%252Foutside", "a//b"] {
        let rejected = client
            .get(harness.url(&format!("/artifacts/{artifact_key}/{suffix}")))
            .send()
            .expect("unsafe Artifact suffix");
        assert!(
            rejected.status().is_client_error(),
            "suffix={suffix}, status={}",
            rejected.status()
        );
    }
    for suffix in ["%2e", "%2e%2e"] {
        let status = raw_get_status(
            harness.address,
            &format!("/artifacts/{artifact_key}/{suffix}"),
        );
        assert!(
            (400..500).contains(&status),
            "suffix={suffix}, status={status}"
        );
    }

    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("route boundary catalogue");
    catalogue
        .execute(
            "UPDATE revisions SET entry_path='../catalogue.sqlite' WHERE id=?1",
            [revision_id],
        )
        .expect("seed unsafe durable entry path");
    let boundary_rejected = client
        .get(
            harness.url(
                url::Url::parse(canonical_artifact)
                    .expect("canonical Artifact URL")
                    .path(),
            ),
        )
        .send()
        .expect("durable entry boundary rejection");
    assert_eq!(boundary_rejected.status(), 500);
    catalogue
        .execute(
            "UPDATE revisions SET entry_path='route.html' WHERE id=?1",
            [revision_id],
        )
        .expect("restore durable entry path");
    drop(catalogue);

    let outside = harness._root.path().join("outside-secret.html");
    fs::write(&outside, "outside secret").expect("outside secret");
    let revision_member = harness
        .storage
        .join("revisions")
        .join(revision_id)
        .join("content")
        .join("route.html");
    fs::remove_file(&revision_member).expect("remove immutable member for tamper fixture");
    std::os::unix::fs::symlink(&outside, &revision_member).expect("tampered member symlink");
    let artifact_open_path = url::Url::parse(canonical_artifact)
        .expect("Artifact URL")
        .path()
        .to_owned();
    let revision_open_path = url::Url::parse(canonical_revision)
        .expect("Revision URL")
        .path()
        .to_owned();
    for path in [&artifact_open_path, &revision_open_path] {
        let response = client
            .get(harness.url(path))
            .send()
            .expect("tampered serving");
        assert_eq!(response.status(), 500, "path={path}");
        assert!(
            !response
                .text()
                .expect("tamper response")
                .contains("outside secret")
        );
    }

    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("route tombstone catalogue");
    catalogue
        .execute(
            "UPDATE artifacts SET state='gone' WHERE id=?1",
            [artifact_id],
        )
        .expect("seed gone Artifact");
    catalogue
        .execute(
            "UPDATE revisions SET state='gone' WHERE id=?1",
            [revision_id],
        )
        .expect("seed gone Revision");
    for (path, code) in [
        (format!("/api/v1/artifacts/{artifact_id}"), "artifact_gone"),
        (format!("/api/v1/revisions/{revision_id}"), "revision_gone"),
        (artifact_open_path, "artifact_gone"),
        (revision_open_path, "revision_gone"),
    ] {
        let gone = client
            .get(harness.url(&path))
            .send()
            .expect("gone identity");
        assert_eq!(gone.status(), 410, "path={path}");
        assert_eq!(
            gone.json::<Value>().expect("gone identity JSON")["error"]["code"],
            code,
            "path={path}"
        );
    }
}

#[test]
fn artifact_discovery_uses_signed_filter_bound_pagination_across_restart() {
    let mut harness = Harness::start();
    let project_directory = harness._root.path().join("artifact-page-project");
    fs::create_dir(&project_directory).expect("Artifact page Project");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-page-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Artifact Pages"}))
        .send()
        .expect("register Artifact page Project")
        .json::<Value>()
        .expect("Artifact page Project result");
    let project_id = project["result"]["id"]
        .as_str()
        .expect("Artifact page Project ID")
        .to_owned();
    for (ordinal, title) in ["Bravo", "Alpha", "Charlie"].into_iter().enumerate() {
        let source = harness._root.path().join(format!("page-{ordinal}.txt"));
        fs::write(&source, format!("{title} body")).expect("Artifact page source");
        let published = client
            .post(harness.url("/api/v1/artifacts"))
            .header("Idempotency-Key", format!("issue-24-page-{ordinal}"))
            .json(&serde_json::json!({
                "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
                "projectId": project_id,
                "title": title,
                "retention": {"mode": "pinned", "ttlMs": null, "pinReason": "pagination"}
            }))
            .send()
            .expect("Publish paginated Artifact");
        assert_eq!(published.status(), 201);
    }
    let invalid_direction = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("direction", "sideways"),
        ])
        .send()
        .expect("invalid Artifact direction");
    assert_eq!(invalid_direction.status(), 422);
    assert_eq!(
        invalid_direction
            .json::<Value>()
            .expect("invalid Artifact direction JSON")["error"]["code"],
        "invalid_direction"
    );
    let first = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("retentionMode", "pinned"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
        ])
        .send()
        .expect("first Artifact page");
    assert_eq!(first.status(), 200);
    assert!(
        first.headers()["link"]
            .to_str()
            .expect("next Link")
            .contains("direction=asc")
    );
    let first = first.json::<Value>().expect("first Artifact page JSON");
    assert_eq!(first["result"]["items"][0]["title"], "Alpha");
    assert_eq!(first["result"]["page"]["hasMore"], true);
    let cursor = first["result"]["page"]["nextCursor"]
        .as_str()
        .expect("Artifact cursor")
        .to_owned();

    let mut tampered = cursor.clone();
    let replacement = if tampered.ends_with('a') { 'b' } else { 'a' };
    tampered.pop();
    tampered.push(replacement);
    let invalid = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("retentionMode", "pinned"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
            ("after", tampered.as_str()),
        ])
        .send()
        .expect("tampered Artifact cursor");
    assert_eq!(invalid.status(), 422);
    assert_eq!(
        invalid.json::<Value>().expect("tampered cursor JSON")["error"]["code"],
        "invalid_cursor"
    );
    let direction_mismatch = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("retentionMode", "pinned"),
            ("order", "title"),
            ("direction", "desc"),
            ("limit", "1"),
            ("after", cursor.as_str()),
        ])
        .send()
        .expect("direction-mismatched Artifact cursor");
    assert_eq!(direction_mismatch.status(), 422);
    let artifact_cursor_on_ledger = client
        .get(harness.url("/api/v1/projects/ledger"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("kind", "artifact"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
            ("after", cursor.as_str()),
        ])
        .send()
        .expect("Artifact cursor on Project ledger");
    assert_eq!(artifact_cursor_on_ledger.status(), 422);
    assert_eq!(
        artifact_cursor_on_ledger
            .json::<Value>()
            .expect("cross-endpoint cursor JSON")["error"]["code"],
        "invalid_cursor"
    );
    let mismatch = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("retentionMode", "pinned"),
            ("order", "recent"),
            ("limit", "1"),
            ("after", cursor.as_str()),
        ])
        .send()
        .expect("filter-mismatched Artifact cursor");
    assert_eq!(mismatch.status(), 422);

    harness.restart_configured(|_| {});
    let second = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("retentionMode", "pinned"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
            ("after", cursor.as_str()),
        ])
        .send()
        .expect("second Artifact page after restart");
    assert_eq!(second.status(), 200);
    let second = second.json::<Value>().expect("second Artifact page JSON");
    assert_eq!(second["result"]["items"][0]["title"], "Bravo");
    let descending = client
        .get(harness.url("/api/v1/artifacts"))
        .query(&[
            ("projectId", project_id.as_str()),
            ("retentionMode", "pinned"),
            ("order", "title"),
            ("direction", "desc"),
            ("limit", "1"),
        ])
        .send()
        .expect("descending Artifact page")
        .json::<Value>()
        .expect("descending Artifact page JSON");
    assert_eq!(descending["result"]["items"][0]["title"], "Charlie");

    for ledger_path in [
        "/api/v1/projects/ledger".to_owned(),
        format!("/api/v1/projects/{project_id}/ledger"),
    ] {
        let mut request = client.get(harness.url(&ledger_path)).query(&[
            ("kind", "artifact"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
        ]);
        if ledger_path == "/api/v1/projects/ledger" {
            request = request.query(&[("projectId", project_id.as_str())]);
        }
        let ledger = request.send().expect("first Project ledger page");
        assert_eq!(ledger.status(), 200);
        let link = ledger.headers()["link"]
            .to_str()
            .expect("Project ledger next Link");
        let target = link
            .strip_prefix('<')
            .and_then(|value| value.split_once('>'))
            .map(|(target, relation)| {
                assert_eq!(relation, "; rel=\"next\"");
                target
            })
            .expect("RFC 8288 Project ledger Link");
        let target = url::Url::parse(target).expect("absolute Project ledger next URL");
        assert_eq!(target.path(), ledger_path);
        assert!(
            target
                .query()
                .expect("ledger next query")
                .contains("direction=asc")
        );
        let continuation_cursor = target
            .query_pairs()
            .find_map(|(name, value)| (name == "after").then(|| value.into_owned()))
            .expect("ledger continuation cursor");
        let wrong_kind = client
            .get(harness.url(&ledger_path))
            .query(&[
                ("kind", "all"),
                ("order", "title"),
                ("direction", "asc"),
                ("limit", "1"),
                ("after", continuation_cursor.as_str()),
            ])
            .send()
            .expect("filter-mismatched ledger cursor");
        assert_eq!(wrong_kind.status(), 422);
        let local_target = format!(
            "{}?{}",
            target.path(),
            target.query().expect("ledger continuation query")
        );
        let continued = client
            .get(harness.url(&local_target))
            .send()
            .expect("continued Project ledger page");
        assert_eq!(continued.status(), 200);
        assert_eq!(
            continued
                .json::<Value>()
                .expect("continued Project ledger JSON")["result"]["items"][0]["title"],
            "Bravo"
        );
    }
}

#[test]
fn artifact_publish_client_timeout_continues_and_identical_retry_replays() {
    let harness = Harness::start();
    let project_directory = harness._root.path().join("artifact-timeout-project");
    fs::create_dir(&project_directory).expect("Artifact timeout Project");
    let source = harness._root.path().join("timeout.html");
    fs::write(&source, "<!doctype html><title>Timeout Artifact</title>")
        .expect("Artifact timeout source");
    let server = format!("http://{}", harness.address);
    let registered = cli(
        &server,
        &[
            "--idempotency-key",
            "issue-24-timeout-project",
            "project",
            "register",
            project_directory.to_str().expect("timeout Project path"),
            "--title",
            "Artifact Timeout",
        ],
    );
    assert!(registered.status.success(), "{registered:?}");
    let registered: Value =
        serde_json::from_slice(&registered.stdout).expect("timeout Project JSON");
    let project_id = registered["result"]["id"]
        .as_str()
        .expect("timeout Project ID")
        .to_owned();
    let mut lock_connection = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("Artifact timeout catalogue lock");
    let lock = lock_connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("hold Artifact timeout write lock");
    let mut command = obs();
    command
        .args([
            "--json",
            "--server",
            &server,
            "--timeout",
            "20ms",
            "-p",
            project_directory.to_str().expect("timeout Project path"),
            "--idempotency-key",
            "issue-24-timeout-publish",
            "artifact",
            "publish",
            source.to_str().expect("timeout source path"),
            "--title",
            "Timeout Artifact",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("timed Artifact Publish CLI");
    thread::sleep(Duration::from_millis(100));
    let timed_out = child
        .wait_with_output()
        .expect("timed Artifact Publish output");
    assert_eq!(timed_out.status.code(), Some(5));
    assert!(timed_out.stdout.is_empty());
    let timeout: Value = serde_json::from_slice(&timed_out.stderr).expect("Publish timeout JSON");
    assert_eq!(timeout["error"]["code"], "client_timeout");
    assert_eq!(
        timeout["error"]["details"]["idempotencyKey"],
        "issue-24-timeout-publish"
    );

    let body = serde_json::json!({
        "source": {
            "path": source,
            "callerWorkingDirectory": std::env::current_dir().expect("test current directory")
        },
        "projectId": project_id,
        "title": "Timeout Artifact",
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let client = reqwest::blocking::Client::new();
    let concurrent = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-timeout-publish")
        .json(&body)
        .send()
        .expect("concurrent Artifact Publish");
    assert_eq!(concurrent.status(), 409);
    assert_eq!(concurrent.headers()["retry-after"], "1");
    assert_eq!(
        concurrent.json::<Value>().expect("concurrent Publish JSON")["error"]["code"],
        "idempotency_in_progress"
    );
    drop(lock);
    drop(lock_connection);

    let deadline = Instant::now() + Duration::from_secs(10);
    let replay = loop {
        let response = client
            .post(harness.url("/api/v1/artifacts"))
            .header("Idempotency-Key", "issue-24-timeout-publish")
            .json(&body)
            .send()
            .expect("retry timed Artifact Publish");
        if response.status() == 201 {
            break response;
        }
        assert_eq!(response.status(), 409);
        assert!(Instant::now() < deadline, "Artifact Publish did not finish");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("Artifact timeout result catalogue");
    let count: u64 = catalogue
        .query_row("SELECT count(*) FROM artifacts", [], |row| row.get(0))
        .expect("Artifact timeout result count");
    assert_eq!(count, 1);
}

#[test]
fn publish_visibility_transaction_failure_rolls_back_and_restart_finishes_once() {
    let mut harness = Harness::start();
    let project_directory = harness._root.path().join("publish-rollback-project");
    fs::create_dir(&project_directory).expect("Publish rollback Project");
    let source = harness._root.path().join("rollback.txt");
    fs::write(&source, "transactional bytes").expect("Publish rollback source");
    let client = reqwest::blocking::Client::new();
    let project = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-24-rollback-project")
        .json(&serde_json::json!({"path": project_directory, "title": "Publish Rollback"}))
        .send()
        .expect("register Publish rollback Project")
        .json::<Value>()
        .expect("Publish rollback Project result");
    let project_id = project["result"]["id"].as_str().expect("Project ID");
    let catalogue_path = harness.storage.join("catalogue.sqlite");
    let catalogue = rusqlite::Connection::open(&catalogue_path).expect("rollback catalogue");
    catalogue
        .execute_batch(
            "CREATE TRIGGER inject_artifact_publish_audit_failure
             BEFORE INSERT ON audit_events
             WHEN NEW.kind='artifact_published'
             BEGIN SELECT RAISE(ABORT, 'injected Artifact Publish audit failure'); END;",
        )
        .expect("install Artifact Publish transaction fault");
    let body = serde_json::json!({
        "source": {"path": source, "callerWorkingDirectory": harness._root.path()},
        "projectId": project_id,
        "retention": {"mode": "default", "ttlMs": null, "pinReason": null}
    });
    let failed = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-rollback-publish")
        .json(&body)
        .send()
        .expect("faulted Publish transaction");
    assert_eq!(failed.status(), 500);
    for table in ["artifacts", "revisions"] {
        let count: u64 = catalogue
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("rolled-back authority count");
        assert_eq!(count, 0, "table={table}");
    }
    let operation_state: String = catalogue
        .query_row(
            "SELECT state FROM operation_intents WHERE kind='artifact_publish'",
            [],
            |row| row.get(0),
        )
        .expect("rolled-back operation state");
    assert_eq!(operation_state, "renamed");
    catalogue
        .execute_batch("DROP TRIGGER inject_artifact_publish_audit_failure;")
        .expect("remove Artifact Publish transaction fault");
    drop(catalogue);

    harness.restart_configured(|_| {});
    let replay = client
        .post(harness.url("/api/v1/artifacts"))
        .header("Idempotency-Key", "issue-24-rollback-publish")
        .json(&body)
        .send()
        .expect("replay transaction-recovered Publish");
    assert_eq!(replay.status(), 201);
    assert_eq!(replay.headers()["idempotency-replayed"], "true");
    let catalogue = rusqlite::Connection::open(catalogue_path).expect("recovered catalogue");
    for table in ["artifacts", "revisions"] {
        let count: u64 = catalogue
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("recovered authority count");
        assert_eq!(count, 1, "table={table}");
    }
    let audit_count: u64 = catalogue
        .query_row(
            "SELECT count(*) FROM audit_events WHERE kind='artifact_published' AND resource_id=(SELECT id FROM artifacts LIMIT 1)",
            [],
            |row| row.get(0),
        )
        .expect("Artifact Publish audit count");
    assert_eq!(audit_count, 1);
}

#[test]
fn project_metadata_update_preserves_identity_and_replays_once() {
    let harness = Harness::start();
    let directory = harness._root.path().join("updated-project");
    fs::create_dir(&directory).expect("updated Project directory");
    let client = reqwest::blocking::Client::new();
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-23-register-update")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("updated Project path"),
            "title": "Original title",
            "slug": "original-title"
        }))
        .send()
        .expect("register Project for update");
    assert_eq!(registered.status(), 201);
    let registered: Value = registered.json().expect("registered Project");
    let project = &registered["result"];
    let id = project["id"].as_str().expect("Project ID").to_owned();
    let canonical_directory = project["canonicalDirectory"].clone();
    let created_at = project["createdAt"].clone();
    let api_url = project["apiUrl"].clone();
    let update = serde_json::json!({
        "title": "Renamed Project",
        "slug": "renamed-project"
    });

    let missing_precondition = client
        .patch(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-update")
        .json(&update)
        .send()
        .expect("update without precondition");
    assert_eq!(missing_precondition.status(), 428);
    assert_eq!(
        missing_precondition
            .json::<Value>()
            .expect("missing precondition error")["error"]["code"],
        "precondition_required"
    );

    let stale = client
        .patch(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-update")
        .header("If-Match", "\"rv-0\"")
        .json(&update)
        .send()
        .expect("stale Project update");
    assert_eq!(stale.status(), 412);
    assert_eq!(
        stale.json::<Value>().expect("stale update error")["error"]["code"],
        "changed_record"
    );

    let updated = client
        .patch(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-update")
        .header("If-Match", "\"rv-1\"")
        .json(&update)
        .send()
        .expect("Project update");
    assert_eq!(updated.status(), 200);
    assert_eq!(updated.headers()["etag"], "\"rv-2\"");
    assert_eq!(updated.headers()["cache-control"], "no-store");
    let updated: Value = updated.json().expect("updated Project");
    let updated_project = &updated["result"];
    assert_eq!(updated_project["id"], id);
    assert_eq!(updated_project["recordVersion"], 2);
    assert_eq!(updated_project["title"], "Renamed Project");
    assert_eq!(updated_project["slug"], "renamed-project");
    assert_eq!(updated_project["canonicalDirectory"], canonical_directory);
    assert_eq!(updated_project["createdAt"], created_at);
    assert_eq!(updated_project["apiUrl"], api_url);
    assert!(
        updated_project["detailUrl"]
            .as_str()
            .expect("updated detail URL")
            .contains("/renamed-project~")
    );

    let replayed = client
        .patch(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-update")
        .header("If-Match", "\"rv-1\"")
        .json(&update)
        .send()
        .expect("replayed Project update");
    assert_eq!(replayed.status(), 200);
    assert_eq!(replayed.headers()["idempotency-replayed"], "true");
    assert_eq!(replayed.headers()["etag"], "\"rv-2\"");
    assert_eq!(replayed.json::<Value>().expect("replayed update"), updated);

    let changed_fingerprint = client
        .patch(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-update")
        .header("If-Match", "\"rv-1\"")
        .json(&serde_json::json!({ "title": "Different title" }))
        .send()
        .expect("changed update fingerprint");
    assert_eq!(changed_fingerprint.status(), 409);
    assert_eq!(
        changed_fingerprint
            .json::<Value>()
            .expect("fingerprint error")["error"]["code"],
        "idempotency_conflict"
    );

    let catalogue =
        rusqlite::Connection::open(harness.storage.join("catalogue.sqlite")).expect("catalogue");
    let update_events: i64 = catalogue
        .query_row(
            "SELECT count(*) FROM audit_events WHERE kind='project_updated' AND resource_id=?1",
            [&id],
            |row| row.get(0),
        )
        .expect("Project update events");
    assert_eq!(update_events, 1);
}

#[test]
fn project_cli_updates_and_tombstones_with_current_preconditions() {
    let harness = Harness::start();
    let directory = harness._root.path().join("cli-lifecycle-project");
    fs::create_dir(&directory).expect("CLI lifecycle Project directory");
    let client = reqwest::blocking::Client::new();
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-23-cli-register")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("CLI lifecycle path"),
            "title": "CLI Lifecycle",
            "slug": "cli-lifecycle"
        }))
        .send()
        .expect("register CLI lifecycle Project")
        .json::<Value>()
        .expect("registered CLI lifecycle Project");
    let key = registered["result"]["key"]
        .as_str()
        .expect("CLI lifecycle key")
        .to_owned();

    let no_idempotency = cli(
        &format!("http://{}", harness.address),
        &["project", "update", &key, "--title", "No key"],
    );
    assert_eq!(no_idempotency.status.code(), Some(2));
    assert!(no_idempotency.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&no_idempotency.stderr).expect("missing key CLI error")["error"]
            ["code"],
        "invalid_idempotency_key"
    );

    let updated = cli(
        &format!("http://{}", harness.address),
        &[
            "--idempotency-key",
            "issue-23-cli-update",
            "project",
            "update",
            &key,
            "--title",
            "CLI Renamed",
            "--slug",
            "cli-renamed",
        ],
    );
    assert!(updated.status.success(), "{:?}", updated.stderr);
    let updated: Value = serde_json::from_slice(&updated.stdout).expect("CLI update result");
    assert_eq!(updated["result"]["title"], "CLI Renamed");
    assert_eq!(updated["result"]["recordVersion"], 2);
    let updated_key = updated["result"]["key"]
        .as_str()
        .expect("updated Project key")
        .to_owned();

    let no_confirmation = cli(
        &format!("http://{}", harness.address),
        &[
            "--idempotency-key",
            "issue-23-cli-tombstone",
            "project",
            "tombstone",
            &updated_key,
        ],
    );
    assert_eq!(no_confirmation.status.code(), Some(2));
    assert!(no_confirmation.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&no_confirmation.stderr).expect("CLI confirmation error")["error"]
            ["code"],
        "confirmation_required"
    );

    let tombstoned = cli(
        &format!("http://{}", harness.address),
        &[
            "--idempotency-key",
            "issue-23-cli-tombstone",
            "project",
            "tombstone",
            &updated_key,
            "--yes",
        ],
    );
    assert!(tombstoned.status.success(), "{:?}", tombstoned.stderr);
    let tombstoned: Value =
        serde_json::from_slice(&tombstoned.stdout).expect("CLI tombstone result");
    assert_eq!(tombstoned["result"]["state"], "gone");
    assert_eq!(tombstoned["result"]["recordVersion"], 3);

    let gone = cli(
        &format!("http://{}", harness.address),
        &["project", "show", &updated_key],
    );
    assert_eq!(gone.status.code(), Some(3));
    assert!(gone.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&gone.stderr).expect("CLI gone result")["error"]["code"],
        "project_gone"
    );

    for (command, idempotency_key) in [
        ("update", "issue-23-cli-update-gone"),
        ("tombstone", "issue-23-cli-tombstone-gone"),
    ] {
        let mut arguments = vec![
            "--idempotency-key",
            idempotency_key,
            "project",
            command,
            &updated_key,
        ];
        if command == "update" {
            arguments.extend(["--title", "Still gone"]);
        } else {
            arguments.push("--yes");
        }
        let result = cli(&format!("http://{}", harness.address), &arguments);
        assert_eq!(result.status.code(), Some(3), "{:?}", result.stderr);
        assert!(result.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stderr).expect("gone mutation result")["error"]
                ["code"],
            "project_gone"
        );
    }

    let unknown = cli(
        &format!("http://{}", harness.address),
        &[
            "--idempotency-key",
            "issue-23-cli-update-unknown",
            "project",
            "update",
            "00000000000000000000000000",
            "--title",
            "Unknown",
        ],
    );
    assert_eq!(unknown.status.code(), Some(3), "{:?}", unknown.stderr);
    assert!(unknown.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&unknown.stderr).expect("unknown mutation result")["error"]
            ["code"],
        "not_found"
    );
}

#[test]
fn project_tombstone_enforces_gates_and_preserves_artifact_association() {
    let harness = Harness::start();
    let directory = harness._root.path().join("tombstone-project");
    fs::create_dir(&directory).expect("tombstone Project directory");
    let client = reqwest::blocking::Client::new();
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-23-register-tombstone")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("tombstone Project path"),
            "title": "Tombstone Project",
            "slug": "tombstone-project"
        }))
        .send()
        .expect("register tombstone Project");
    assert_eq!(registered.status(), 201);
    let registered: Value = registered.json().expect("registered tombstone Project");
    let project = &registered["result"];
    let id = project["id"].as_str().expect("Project ID").to_owned();
    let key = project["key"].as_str().expect("Project key").to_owned();
    let detail_url = project["detailUrl"].clone();
    let api_url = project["apiUrl"].clone();
    let catalogue_path = harness.storage.join("catalogue.sqlite");
    let catalogue = rusqlite::Connection::open(&catalogue_path).expect("catalogue");
    catalogue
        .execute(
            "INSERT INTO artifacts(
                 id,project_id,record_version,state,title,description,slug,title_fold,search_text,
                 retention_mode,files,logical_bytes,revision_count,published_at,updated_at
             ) VALUES (
                 ?1,?2,1,'live','Fixture Artifact','','fixture-artifact','fixture artifact',
                 'fixture artifact','default',1,0,1,
                 '2026-07-10T00:00:00.000Z','2026-07-10T00:00:00.000Z'
             )",
            rusqlite::params!["11111111111111111111111111", id],
        )
        .expect("associated Artifact fixture");
    catalogue
        .execute(
            "INSERT INTO services(id,project_id,record_version,state) VALUES (?1,?2,1,'live')",
            rusqlite::params!["22222222222222222222222222", id],
        )
        .expect("live Service fixture");
    catalogue
        .execute(
            "INSERT INTO operation_intents(id,kind,state,details_json,project_id)
             VALUES (?1,'fixture','intent_recorded','{}',?2)",
            rusqlite::params!["33333333333333333333333333", id],
        )
        .expect("Project operation fixture");
    drop(catalogue);
    let tombstone_url = harness.url(&format!("/api/v1/projects/{id}"));
    let confirmation = serde_json::json!({ "confirmation": key });

    let missing_precondition = client
        .delete(&tombstone_url)
        .header("Idempotency-Key", "issue-23-tombstone")
        .json(&confirmation)
        .send()
        .expect("tombstone without precondition");
    assert_eq!(missing_precondition.status(), 428);

    let wrong_confirmation = client
        .delete(&tombstone_url)
        .header("Idempotency-Key", "issue-23-tombstone")
        .header("If-Match", "\"rv-1\"")
        .json(&serde_json::json!({ "confirmation": "wrong-project" }))
        .send()
        .expect("tombstone with wrong confirmation");
    assert_eq!(wrong_confirmation.status(), 422);
    assert_eq!(
        wrong_confirmation
            .json::<Value>()
            .expect("confirmation error")["error"]["code"],
        "confirmation_required"
    );

    let live_service = client
        .delete(&tombstone_url)
        .header("Idempotency-Key", "issue-23-tombstone")
        .header("If-Match", "\"rv-1\"")
        .json(&confirmation)
        .send()
        .expect("tombstone with live Service");
    assert_eq!(live_service.status(), 409);
    assert_eq!(
        live_service.json::<Value>().expect("live Service error")["error"]["code"],
        "project_has_live_services"
    );

    let catalogue = rusqlite::Connection::open(&catalogue_path).expect("catalogue");
    catalogue
        .execute(
            "UPDATE services SET state='gone' WHERE project_id=?1",
            [&id],
        )
        .expect("retire Service fixture");
    drop(catalogue);
    let active_operation = client
        .delete(&tombstone_url)
        .header("Idempotency-Key", "issue-23-tombstone")
        .header("If-Match", "\"rv-1\"")
        .json(&confirmation)
        .send()
        .expect("tombstone with active operation");
    assert_eq!(active_operation.status(), 409);
    assert_eq!(
        active_operation
            .json::<Value>()
            .expect("active operation error")["error"]["code"],
        "project_operation_in_progress"
    );

    let catalogue = rusqlite::Connection::open(&catalogue_path).expect("catalogue");
    catalogue
        .execute(
            "UPDATE operation_intents SET state='completed' WHERE project_id=?1",
            [&id],
        )
        .expect("complete Project operation fixture");
    drop(catalogue);
    let tombstoned = client
        .delete(&tombstone_url)
        .header("Idempotency-Key", "issue-23-tombstone")
        .header("If-Match", "\"rv-1\"")
        .json(&confirmation)
        .send()
        .expect("tombstone Project");
    assert_eq!(tombstoned.status(), 200);
    assert_eq!(tombstoned.headers()["etag"], "\"rv-2\"");
    let tombstoned: Value = tombstoned.json().expect("tombstoned Project result");
    assert_eq!(tombstoned["result"]["id"], id);
    assert_eq!(tombstoned["result"]["key"], key);
    assert_eq!(tombstoned["result"]["state"], "gone");
    assert_eq!(tombstoned["result"]["recordVersion"], 2);
    assert_eq!(tombstoned["result"]["terminalState"], "tombstoned");
    assert_eq!(tombstoned["result"]["cause"], "operator");
    assert_eq!(tombstoned["result"]["detailUrl"], detail_url);
    assert_eq!(tombstoned["result"]["apiUrl"], api_url);
    assert!(tombstoned["result"]["tombstonedAt"].is_string());

    let replayed = client
        .delete(&tombstone_url)
        .header("Idempotency-Key", "issue-23-tombstone")
        .header("If-Match", "\"rv-1\"")
        .json(&confirmation)
        .send()
        .expect("replay Project tombstone");
    assert_eq!(replayed.status(), 200);
    assert_eq!(replayed.headers()["idempotency-replayed"], "true");
    assert_eq!(
        replayed.json::<Value>().expect("replayed tombstone"),
        tombstoned
    );

    let gone = client
        .get(&tombstone_url)
        .send()
        .expect("show tombstoned Project");
    assert_eq!(gone.status(), 410);
    assert_eq!(
        gone.json::<Value>().expect("gone Project error")["error"]["code"],
        "project_gone"
    );
    let cannot_reuse = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-23-reuse-tombstone")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("tombstone Project path")
        }))
        .send()
        .expect("attempt Project identity reuse");
    assert_eq!(cannot_reuse.status(), 410);

    let catalogue = rusqlite::Connection::open(catalogue_path).expect("catalogue");
    let artifact_project: String = catalogue
        .query_row(
            "SELECT project_id FROM artifacts WHERE id='11111111111111111111111111'",
            [],
            |row| row.get(0),
        )
        .expect("Artifact Project association");
    assert_eq!(artifact_project, id);
    let tombstone_events: i64 = catalogue
        .query_row(
            "SELECT count(*) FROM audit_events WHERE kind='project_tombstoned' AND resource_id=?1",
            [&id],
            |row| row.get(0),
        )
        .expect("Project tombstone audit count");
    assert_eq!(tombstone_events, 1);
}

#[test]
fn project_tombstone_audit_failure_rolls_back_and_retries_after_restart() {
    let mut harness = Harness::start();
    let directory = harness._root.path().join("tombstone-rollback-project");
    fs::create_dir(&directory).expect("tombstone rollback Project directory");
    let client = reqwest::blocking::Client::new();
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-23-tombstone-rollback-register")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("tombstone rollback path"),
            "title": "Tombstone Rollback",
            "slug": "tombstone-rollback"
        }))
        .send()
        .expect("register tombstone rollback Project")
        .json::<Value>()
        .expect("tombstone rollback Project");
    let id = registered["result"]["id"]
        .as_str()
        .expect("tombstone rollback ID")
        .to_owned();
    let key = registered["result"]["key"]
        .as_str()
        .expect("tombstone rollback key")
        .to_owned();
    let catalogue = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("tombstone fault catalogue");
    catalogue
        .execute_batch(
            "CREATE TRIGGER inject_project_tombstone_failure
             BEFORE INSERT ON audit_events
             WHEN NEW.cause = 'project_tombstoned'
             BEGIN SELECT RAISE(ABORT, 'injected tombstone failure'); END;",
        )
        .expect("install tombstone failure trigger");
    let body = serde_json::json!({ "confirmation": key });
    let failed = client
        .delete(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-tombstone-rollback")
        .header("If-Match", "\"rv-1\"")
        .json(&body)
        .send()
        .expect("injected failed tombstone");
    assert_eq!(failed.status(), 500);
    let still_live = client
        .get(harness.url(&format!("/api/v1/projects/{id}")))
        .send()
        .expect("Project after failed tombstone");
    assert_eq!(still_live.status(), 200);
    assert_eq!(still_live.headers()["etag"], "\"rv-1\"");
    catalogue
        .execute_batch("DROP TRIGGER inject_project_tombstone_failure;")
        .expect("remove tombstone failure trigger");
    drop(catalogue);

    let pid = harness.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("SIGTERM")
            .success()
    );
    assert!(harness.child.wait().expect("stopped daemon").success());
    harness.child = daemon(&harness.runtime, &harness.storage, harness.address)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("restart daemon after tombstone failure");
    harness.wait_ready();

    let retried = client
        .delete(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-tombstone-rollback")
        .header("If-Match", "\"rv-1\"")
        .json(&body)
        .send()
        .expect("retry tombstone after restart");
    assert_eq!(retried.status(), 200);
    assert!(retried.headers().get("idempotency-replayed").is_none());
    assert_eq!(
        retried.json::<Value>().expect("retried tombstone")["result"]["state"],
        "gone"
    );
}

#[test]
fn project_discovery_paginates_searches_shows_and_reads_empty_ledgers() {
    let mut harness = Harness::start();
    let client = reqwest::blocking::Client::new();
    let mut projects = Vec::new();
    for (index, title) in ["Zulu", "Straße Atlas", "Alpha"].into_iter().enumerate() {
        let directory = harness._root.path().join(format!("project-{index}"));
        fs::create_dir(&directory).expect("Project directory");
        let response = client
            .post(harness.url("/api/v1/projects"))
            .header("Idempotency-Key", format!("issue-22-list-{index}"))
            .json(&serde_json::json!({ "path": directory, "title": title }))
            .send()
            .expect("register listed Project");
        assert_eq!(response.status(), 201);
        projects.push(response.json::<Value>().expect("Project JSON")["result"].clone());
        thread::sleep(Duration::from_millis(2));
    }

    let searched = client
        .get(harness.url("/api/v1/projects"))
        .query(&[("query", "STRASSE")])
        .send()
        .expect("search Projects");
    assert_eq!(searched.status(), 200);
    let searched: Value = searched.json().expect("search JSON");
    assert_eq!(
        searched["result"]["items"].as_array().expect("items").len(),
        1
    );
    assert_eq!(searched["result"]["items"][0]["title"], "Straße Atlas");

    let first_page = client
        .get(harness.url("/api/v1/projects"))
        .query(&[
            ("state", "live"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
        ])
        .send()
        .expect("first Project page");
    assert_eq!(first_page.status(), 200);
    let link = first_page.headers()["link"]
        .to_str()
        .expect("next Link")
        .to_owned();
    assert!(link.starts_with("<https://desktop.greyhound-chinstrap.ts.net/api/v1/projects?"));
    assert!(link.ends_with(">; rel=\"next\""));
    let first_page: Value = first_page.json().expect("first page JSON");
    assert_eq!(first_page["result"]["items"][0]["title"], "Alpha");
    assert_eq!(first_page["result"]["page"]["limit"], 1);
    assert_eq!(first_page["result"]["page"]["hasMore"], true);
    let cursor = first_page["result"]["page"]["nextCursor"]
        .as_str()
        .expect("next cursor")
        .to_owned();
    let mut tampered_cursor = cursor.clone();
    tampered_cursor.push('x');
    let tampered = client
        .get(harness.url("/api/v1/projects"))
        .query(&[("after", tampered_cursor)])
        .send()
        .expect("tampered cursor");
    assert_eq!(tampered.status(), 422);
    assert_eq!(
        tampered.json::<Value>().expect("tampered cursor JSON")["error"]["code"],
        "invalid_cursor"
    );

    let pid = harness.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("SIGTERM")
            .success()
    );
    assert!(harness.child.wait().expect("stopped daemon").success());
    harness.child = daemon(&harness.runtime, &harness.storage, harness.address)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("restart daemon");
    harness.wait_ready();

    let second_page = client
        .get(harness.url("/api/v1/projects"))
        .query(&[
            ("state", "live"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
            ("after", cursor.as_str()),
        ])
        .send()
        .expect("second Project page");
    assert_eq!(second_page.status(), 200);
    let second_page: Value = second_page.json().expect("second page JSON");
    assert_eq!(second_page["result"]["items"][0]["title"], "Straße Atlas");

    let mismatched = client
        .get(harness.url("/api/v1/projects"))
        .query(&[
            ("query", "different"),
            ("state", "live"),
            ("order", "title"),
            ("direction", "asc"),
            ("limit", "1"),
            ("after", cursor.as_str()),
        ])
        .send()
        .expect("mismatched cursor");
    assert_eq!(mismatched.status(), 422);
    let mismatched: Value = mismatched.json().expect("cursor error JSON");
    assert_eq!(mismatched["error"]["code"], "invalid_cursor");

    let id = projects[0]["id"].as_str().expect("Project ID");
    let shown = client
        .get(harness.url(&format!("/api/v1/projects/{id}")))
        .send()
        .expect("show Project");
    assert_eq!(shown.status(), 200);
    assert_eq!(shown.headers()["etag"], "\"rv-1\"");
    assert_eq!(
        shown.json::<Value>().expect("show JSON")["result"],
        projects[0]
    );

    let malformed = client
        .get(harness.url(&format!("/api/v1/projects/{}", id.to_ascii_uppercase())))
        .send()
        .expect("malformed Project ID");
    assert_eq!(malformed.status(), 422);
    let unknown = client
        .get(harness.url("/api/v1/projects/00000000000000000000000000"))
        .send()
        .expect("unknown Project ID");
    assert_eq!(unknown.status(), 404);

    for path in [
        "/api/v1/projects/ledger".to_owned(),
        format!("/api/v1/projects/{id}/ledger"),
    ] {
        let ledger = client
            .get(harness.url(&path))
            .send()
            .expect("empty Project ledger");
        assert_eq!(ledger.status(), 200);
        assert!(ledger.headers().get("link").is_none());
        let ledger: Value = ledger.json().expect("ledger JSON");
        assert_eq!(ledger["result"]["items"], serde_json::json!([]));
        assert_eq!(ledger["result"]["page"]["hasMore"], false);
    }
}

#[test]
fn project_resolve_and_discovery_distinguish_malformed_unknown_and_gone() {
    let mut harness = Harness::start();
    let gone_directory = harness._root.path().join("gone-project");
    fs::create_dir(&gone_directory).expect("gone Project directory");
    let canonical_directory = fs::canonicalize(&gone_directory)
        .expect("canonical gone directory")
        .to_str()
        .expect("gone directory UTF-8")
        .to_owned();
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
        .expect("gone fixture catalogue");
    connection
        .execute(
            "INSERT INTO projects(
                 id, record_version, canonical_directory, state, title, slug,
                 title_fold, search_text, created_at, updated_at,
                 terminal_state, tombstoned_at, cause
             ) VALUES (?1, 2, ?2, 'gone', 'Gone Project', 'gone-project',
                 'gone project', ?3, '2026-01-01T00:00:00.000Z',
                 '2026-01-02T00:00:00.000Z', 'tombstoned',
                 '2026-01-02T00:00:00.000Z', 'operator')",
            rusqlite::params![
                "00000000000000000000000000",
                canonical_directory,
                format!("gone project\ngone-project\n{canonical_directory}"),
            ],
        )
        .expect("gone Project fixture");
    drop(connection);
    harness.child = daemon(&harness.runtime, &harness.storage, harness.address)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("restart daemon");
    harness.wait_ready();

    let client = reqwest::blocking::Client::new();
    let gone = client
        .get(harness.url("/api/v1/projects/resolve"))
        .query(&[("path", gone_directory.to_str().expect("gone path"))])
        .send()
        .expect("resolve gone Project");
    assert_eq!(gone.status(), 200);
    let gone: Value = gone.json().expect("gone resolve JSON");
    assert_eq!(gone["result"]["status"], "gone");
    assert_eq!(
        gone["result"]["project"]["id"],
        "00000000000000000000000000"
    );

    let gone_list = client
        .get(harness.url("/api/v1/projects"))
        .query(&[("state", "gone")])
        .send()
        .expect("list gone Projects");
    assert_eq!(gone_list.status(), 200);
    let gone_list: Value = gone_list.json().expect("gone list JSON");
    assert_eq!(gone_list["result"]["items"][0]["state"], "gone");
    assert_eq!(
        gone_list["result"]["items"][0]["terminalState"],
        "tombstoned"
    );
    assert_eq!(gone_list["result"]["items"][0]["cause"], "operator");

    let shown = client
        .get(harness.url("/api/v1/projects/00000000000000000000000000"))
        .send()
        .expect("show gone Project");
    assert_eq!(shown.status(), 410);
    assert_eq!(
        shown.json::<Value>().expect("gone show JSON")["error"]["code"],
        "project_gone"
    );
    let reused = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-gone-reuse")
        .json(&serde_json::json!({ "path": gone_directory }))
        .send()
        .expect("register gone directory");
    assert_eq!(reused.status(), 410);

    let malformed = client
        .get(harness.url("/api/v1/projects/resolve"))
        .query(&[("path", harness._root.path().join("missing"))])
        .send()
        .expect("resolve malformed Project path");
    assert_eq!(malformed.status(), 422);
    assert_eq!(
        malformed.json::<Value>().expect("malformed resolve JSON")["error"]["code"],
        "invalid_project_directory"
    );
}

#[test]
fn project_cli_resolves_registers_lists_and_shows_through_the_daemon() {
    let harness = Harness::start();
    let server = format!("http://{}", harness.address);
    let directory = harness._root.path().join("cli-project");
    fs::create_dir(&directory).expect("CLI Project directory");
    let directory = directory.to_str().expect("CLI Project path");

    let unresolved = cli(&server, &["-p", directory, "project", "resolve"]);
    assert!(unresolved.status.success());
    let unresolved: Value = serde_json::from_slice(&unresolved.stdout).expect("resolve JSON");
    assert_eq!(unresolved["result"]["status"], "unregistered");

    let missing_key = cli(
        &server,
        &["project", "register", directory, "--title", "CLI Project"],
    );
    assert_eq!(missing_key.status.code(), Some(2));
    assert!(missing_key.stdout.is_empty());
    let missing_key: Value =
        serde_json::from_slice(&missing_key.stderr).expect("idempotency error JSON");
    assert_eq!(missing_key["error"]["code"], "invalid_idempotency_key");

    let registered = cli(
        &server,
        &[
            "--idempotency-key",
            "issue-22-cli-register",
            "project",
            "register",
            directory,
            "--title",
            "CLI Project",
        ],
    );
    assert!(registered.status.success(), "{:?}", registered);
    let registered: Value = serde_json::from_slice(&registered.stdout).expect("registration JSON");
    let project = &registered["result"];
    let id = project["id"].as_str().expect("Project ID");
    let key = project["key"].as_str().expect("Project key");

    let listed = cli(
        &server,
        &["project", "list", "--query", "cli", "--order", "title"],
    );
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).expect("list JSON");
    assert_eq!(listed["result"]["items"][0]["id"], id);

    for selector in [id, key, directory] {
        let shown = cli(&server, &["project", "show", selector]);
        assert!(shown.status.success(), "selector={selector}: {shown:?}");
        let shown: Value = serde_json::from_slice(&shown.stdout).expect("show JSON");
        assert_eq!(shown["result"]["id"], id);
    }
}

#[test]
fn project_registration_timeout_continues_and_identical_retry_replays() {
    let harness = Harness::start();
    let directory = harness._root.path().join("timeout-project");
    fs::create_dir(&directory).expect("timeout Project directory");
    let body = serde_json::json!({
        "path": directory,
        "title": "Timeout Project"
    });

    let mut lock_connection = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("lock catalogue");
    let lock = lock_connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("hold write lock");

    let server = format!("http://{}", harness.address);
    let mut command = obs();
    command
        .args([
            "--json",
            "--server",
            &server,
            "--timeout",
            "20ms",
            "--idempotency-key",
            "issue-22-timeout-replay",
            "project",
            "register",
            directory.to_str().expect("timeout Project path"),
            "--title",
            "Timeout Project",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("timed registration CLI");
    thread::sleep(Duration::from_millis(100));
    let timed_out = child.wait_with_output().expect("timed registration output");
    assert_eq!(timed_out.status.code(), Some(5));
    assert!(timed_out.stdout.is_empty());
    let timeout: Value = serde_json::from_slice(&timed_out.stderr).expect("timeout JSON");
    assert_eq!(timeout["error"]["code"], "client_timeout");
    assert!(
        timeout["error"]["message"]
            .as_str()
            .expect("timeout message")
            .contains("same Idempotency-Key")
    );
    assert_eq!(
        timeout["error"]["details"]["idempotencyKey"],
        "issue-22-timeout-replay"
    );
    assert_eq!(
        timeout["error"]["details"]["retry"],
        "repeat the identical request with the same key"
    );

    let client = reqwest::blocking::Client::new();
    let in_progress = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-timeout-replay")
        .json(&body)
        .send()
        .expect("in-progress retry");
    assert_eq!(in_progress.status(), 409);
    assert_eq!(in_progress.headers()["retry-after"], "1");
    let in_progress: Value = in_progress.json().expect("in-progress JSON");
    assert_eq!(in_progress["error"]["code"], "idempotency_in_progress");

    lock.rollback().expect("release write lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let replay = client
            .post(harness.url("/api/v1/projects"))
            .header("Idempotency-Key", "issue-22-timeout-replay")
            .json(&body)
            .send()
            .expect("retry registration");
        if replay.status() == 201 {
            assert_eq!(replay.headers()["idempotency-replayed"], "true");
            break;
        }
        assert_eq!(replay.status(), 409);
        assert!(Instant::now() < deadline, "registration did not complete");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn project_update_timeout_continues_and_identical_retry_replays() {
    let harness = Harness::start();
    let directory = harness._root.path().join("update-timeout-project");
    fs::create_dir(&directory).expect("update timeout Project directory");
    let client = reqwest::blocking::Client::new();
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-23-timeout-register")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("update timeout path"),
            "title": "Before timeout",
            "slug": "before-timeout"
        }))
        .send()
        .expect("register update timeout Project")
        .json::<Value>()
        .expect("update timeout Project");
    let id = registered["result"]["id"]
        .as_str()
        .expect("update timeout ID")
        .to_owned();
    let key = registered["result"]["key"]
        .as_str()
        .expect("update timeout key")
        .to_owned();
    let body = serde_json::json!({
        "title": "After timeout",
        "slug": "after-timeout"
    });

    let mut lock_connection = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("lock update catalogue");
    let lock = lock_connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("hold update write lock");
    let server = format!("http://{}", harness.address);
    let mut command = obs();
    command
        .args([
            "--json",
            "--server",
            &server,
            "--timeout",
            "20ms",
            "--idempotency-key",
            "issue-23-update-timeout",
            "project",
            "update",
            &key,
            "--title",
            "After timeout",
            "--slug",
            "after-timeout",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("timed Project update CLI");
    thread::sleep(Duration::from_millis(100));
    let timed_out = child.wait_with_output().expect("timed update output");
    assert_eq!(timed_out.status.code(), Some(5));
    assert!(timed_out.stdout.is_empty());
    let timeout: Value = serde_json::from_slice(&timed_out.stderr).expect("update timeout JSON");
    assert_eq!(timeout["error"]["code"], "client_timeout");
    assert_eq!(
        timeout["error"]["details"]["idempotencyKey"],
        "issue-23-update-timeout"
    );

    let in_progress = client
        .patch(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-update-timeout")
        .header("If-Match", "\"rv-1\"")
        .json(&body)
        .send()
        .expect("in-progress update retry");
    assert_eq!(in_progress.status(), 409);
    assert_eq!(in_progress.headers()["retry-after"], "1");
    assert_eq!(
        in_progress.json::<Value>().expect("in-progress update")["error"]["code"],
        "idempotency_in_progress"
    );

    lock.rollback().expect("release update write lock");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let replay = client
            .patch(harness.url(&format!("/api/v1/projects/{id}")))
            .header("Idempotency-Key", "issue-23-update-timeout")
            .header("If-Match", "\"rv-1\"")
            .json(&body)
            .send()
            .expect("retry Project update");
        if replay.status() == 200 {
            assert_eq!(replay.headers()["idempotency-replayed"], "true");
            assert_eq!(
                replay.json::<Value>().expect("replayed Project update")["result"]["recordVersion"],
                2
            );
            break;
        }
        assert_eq!(replay.status(), 409);
        assert!(Instant::now() < deadline, "Project update did not complete");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn project_registration_transaction_failure_rolls_back_and_retries_after_restart() {
    let mut harness = Harness::start();
    let directory = harness._root.path().join("rollback-project");
    fs::create_dir(&directory).expect("rollback Project directory");
    let connection = rusqlite::Connection::open(harness.storage.join("catalogue.sqlite"))
        .expect("fault-injection catalogue");
    connection
        .execute_batch(
            "CREATE TRIGGER inject_project_registration_failure
             BEFORE INSERT ON audit_events
             WHEN NEW.cause = 'project_registered'
             BEGIN SELECT RAISE(ABORT, 'injected registration failure'); END;",
        )
        .expect("install registration failure trigger");
    let client = reqwest::blocking::Client::new();
    let body = serde_json::json!({
        "path": directory,
        "title": "Rollback Project"
    });
    let failed = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-rollback-retry")
        .json(&body)
        .send()
        .expect("injected failed registration");
    assert_eq!(failed.status(), 500);

    let resolved = client
        .get(harness.url("/api/v1/projects/resolve"))
        .query(&[("path", directory.to_str().expect("rollback path"))])
        .send()
        .expect("resolve after rollback");
    assert_eq!(resolved.status(), 200);
    assert_eq!(
        resolved.json::<Value>().expect("resolve JSON")["result"]["status"],
        "unregistered"
    );
    connection
        .execute_batch("DROP TRIGGER inject_project_registration_failure;")
        .expect("remove registration failure trigger");
    drop(connection);

    let pid = harness.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("SIGTERM")
            .success()
    );
    assert!(harness.child.wait().expect("stopped daemon").success());
    harness.child = daemon(&harness.runtime, &harness.storage, harness.address)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("restart daemon");
    harness.wait_ready();

    let retried = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-22-rollback-retry")
        .json(&body)
        .send()
        .expect("retry rolled-back registration");
    assert_eq!(retried.status(), 201);
    assert!(retried.headers().get("idempotency-replayed").is_none());
}

#[test]
fn browser_project_registration_is_same_origin_one_use_and_progressively_enhanced() {
    let harness = Harness::start();
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("browser client");
    let form_page = client
        .get(harness.url("/ui/projects/new/"))
        .send()
        .expect("registration form");
    assert_eq!(form_page.status(), 200);
    assert_eq!(form_page.headers()["cache-control"], "no-store");
    let form_page = form_page.text().expect("registration HTML");
    assert!(form_page.contains("<form"));
    assert!(form_page.contains("method=\"post\""));
    assert!(form_page.contains("data-project-registration"));
    let csrf = hidden_form_value(&form_page, "csrfToken");
    let idempotency_key = hidden_form_value(&form_page, "idempotencyKey");
    let directory = harness._root.path().join("browser-project");
    fs::create_dir(&directory).expect("browser Project directory");
    let form = [
        ("path", directory.to_str().expect("browser Project path")),
        ("title", "Browser Project"),
        ("slug", ""),
        ("csrfToken", &csrf),
        ("idempotencyKey", &idempotency_key),
    ];

    let rejected = client
        .post(harness.url("/ui/projects/"))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://attacker.example")
        .header("Sec-Fetch-Site", "cross-site")
        .form(&form)
        .send()
        .expect("cross-origin form");
    assert_eq!(rejected.status(), 403);
    assert!(
        rejected
            .text()
            .expect("rejection HTML")
            .contains("browser_origin_rejected")
    );

    let conflicting_source = client
        .post(harness.url("/ui/projects/"))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://attacker.example")
        .header(
            "Referer",
            "https://desktop.greyhound-chinstrap.ts.net/ui/projects/new/",
        )
        .header("Sec-Fetch-Site", "same-origin")
        .form(&form)
        .send()
        .expect("conflicting browser source headers");
    assert_eq!(conflicting_source.status(), 403);
    assert!(
        conflicting_source
            .text()
            .expect("conflicting source rejection HTML")
            .contains("browser_origin_rejected")
    );

    let api_without_csrf = client
        .post(harness.url("/api/v1/projects"))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://desktop.greyhound-chinstrap.ts.net")
        .header("Sec-Fetch-Site", "same-origin")
        .header("Idempotency-Key", "browser-api-without-csrf")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("browser Project path"),
            "title": "Browser Project"
        }))
        .send()
        .expect("browser API mutation without CSRF");
    assert_eq!(api_without_csrf.status(), 403);
    let api_error: Value = api_without_csrf.json().expect("browser API error envelope");
    assert_eq!(api_error["error"]["code"], "csrf_rejected");

    let accepted = client
        .post(harness.url("/ui/projects/"))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://desktop.greyhound-chinstrap.ts.net")
        .header("Sec-Fetch-Site", "same-origin")
        .form(&form)
        .send()
        .expect("same-origin form");
    assert_eq!(accepted.status(), 303);
    let location = accepted.headers()["location"]
        .to_str()
        .expect("canonical detail Location")
        .to_owned();
    assert!(
        location
            .starts_with("https://desktop.greyhound-chinstrap.ts.net/ui/projects/browser-project~")
    );
    assert!(location.ends_with('/'));
    let detail_url = url::Url::parse(&location).expect("detail URL");
    let stale_path = detail_url.path().replacen("browser-project~", "stale~", 1);
    let stale = client
        .get(harness.url(&stale_path))
        .send()
        .expect("stale Project slug");
    assert_eq!(stale.status(), 308);
    assert_eq!(stale.headers()["location"], location);
    let project_id = detail_url
        .path()
        .trim_end_matches('/')
        .rsplit_once('~')
        .map(|(_, id)| id)
        .expect("detail Project ID");
    let uppercase_path = detail_url
        .path()
        .replace(project_id, &project_id.to_ascii_uppercase());
    let uppercase = client
        .get(harness.url(&uppercase_path))
        .send()
        .expect("uppercase browser Project ID");
    assert_eq!(uppercase.status(), 308);
    assert_eq!(uppercase.headers()["location"], location);

    let replayed = client
        .post(harness.url("/ui/projects/"))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header(
            "Referer",
            "https://desktop.greyhound-chinstrap.ts.net/ui/projects/new/",
        )
        .header("Sec-Fetch-Site", "same-origin")
        .form(&form)
        .send()
        .expect("replayed form");
    assert_eq!(replayed.status(), 403);
    assert!(replayed.text().expect("replay HTML").contains("replayed"));

    let detail_path = detail_url.path().to_owned();
    let detail = client
        .get(harness.url(&detail_path))
        .send()
        .expect("Project detail");
    assert_eq!(detail.status(), 200);
    let detail = detail.text().expect("detail HTML");
    assert!(detail.contains("Browser Project"));
    assert!(detail.contains("No Entries yet"));
    let index = client
        .get(harness.url("/ui/"))
        .send()
        .expect("Project index")
        .text()
        .expect("index HTML");
    assert!(index.contains("Browser Project"));
    assert!(!index.contains("No projects yet"));

    let enhanced_form = client
        .get(harness.url("/ui/projects/new/"))
        .send()
        .expect("enhanced registration form")
        .text()
        .expect("enhanced form HTML");
    let enhanced_csrf = hidden_form_value(&enhanced_form, "csrfToken");
    let enhanced_key = hidden_form_value(&enhanced_form, "idempotencyKey");
    let enhanced_directory = harness._root.path().join("enhanced-project");
    fs::create_dir(&enhanced_directory).expect("enhanced Project directory");
    let enhanced = client
        .post(harness.url("/ui/projects/"))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://desktop.greyhound-chinstrap.ts.net")
        .header("Sec-Fetch-Site", "same-origin")
        .header("X-Observatory-CSRF", &enhanced_csrf)
        .header("X-Observatory-Enhanced", "fetch")
        .form(&[
            (
                "path",
                enhanced_directory.to_str().expect("enhanced Project path"),
            ),
            ("title", "Enhanced Project"),
            ("slug", ""),
            ("csrfToken", &enhanced_csrf),
            ("idempotencyKey", &enhanced_key),
        ])
        .send()
        .expect("enhanced form submission");
    assert_eq!(enhanced.status(), 303);
}

#[test]
fn browser_project_update_and_tombstone_bind_version_and_confirmation() {
    let harness = Harness::start();
    let directory = harness._root.path().join("browser-lifecycle-project");
    fs::create_dir(&directory).expect("browser lifecycle Project directory");
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("browser lifecycle client");
    let registered = client
        .post(harness.url("/api/v1/projects"))
        .header("Idempotency-Key", "issue-23-browser-register")
        .json(&serde_json::json!({
            "path": directory.to_str().expect("browser lifecycle path"),
            "title": "Browser Lifecycle",
            "slug": "browser-lifecycle"
        }))
        .send()
        .expect("register browser lifecycle Project")
        .json::<Value>()
        .expect("browser lifecycle Project");
    let id = registered["result"]["id"]
        .as_str()
        .expect("browser lifecycle ID")
        .to_owned();
    let detail_url = registered["result"]["detailUrl"]
        .as_str()
        .expect("browser lifecycle detail URL");
    let detail_path = url::Url::parse(detail_url)
        .expect("browser lifecycle detail URL")
        .path()
        .to_owned();

    let stale_page = client
        .get(harness.url(&detail_path))
        .send()
        .expect("initial lifecycle detail")
        .text()
        .expect("initial lifecycle HTML");
    assert!(stale_page.contains("data-project-update"));
    assert!(stale_page.contains("Tombstone Project"));
    let stale_csrf = hidden_form_value(&stale_page, "csrfToken");
    let stale_key = hidden_form_value(&stale_page, "idempotencyKey");
    let stale_if_match = hidden_form_value(&stale_page, "ifMatch");
    assert_eq!(stale_if_match, "\"rv-1\"");

    let concurrent = client
        .patch(harness.url(&format!("/api/v1/projects/{id}")))
        .header("Idempotency-Key", "issue-23-browser-concurrent")
        .header("If-Match", "\"rv-1\"")
        .json(&serde_json::json!({ "title": "Concurrent title" }))
        .send()
        .expect("concurrent Project update");
    assert_eq!(concurrent.status(), 200);

    let stale_submission = client
        .post(harness.url(&format!("{detail_path}update/")))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://desktop.greyhound-chinstrap.ts.net")
        .header("Sec-Fetch-Site", "same-origin")
        .form(&[
            ("title", "Stale title"),
            ("slug", "browser-lifecycle"),
            ("csrfToken", &stale_csrf),
            ("idempotencyKey", &stale_key),
            ("ifMatch", &stale_if_match),
        ])
        .send()
        .expect("stale browser update");
    assert_eq!(stale_submission.status(), 403);
    assert!(
        stale_submission
            .text()
            .expect("stale browser error")
            .contains("csrf_rejected")
    );

    let fresh_page = client
        .get(harness.url(&detail_path))
        .send()
        .expect("fresh lifecycle detail")
        .text()
        .expect("fresh lifecycle HTML");
    let update_csrf = hidden_form_value(&fresh_page, "csrfToken");
    let update_key = hidden_form_value(&fresh_page, "idempotencyKey");
    let update_if_match = hidden_form_value(&fresh_page, "ifMatch");
    assert_eq!(update_if_match, "\"rv-2\"");
    let updated = client
        .post(harness.url(&format!("{detail_path}update/")))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://desktop.greyhound-chinstrap.ts.net")
        .header("Sec-Fetch-Site", "same-origin")
        .form(&[
            ("title", "Browser Renamed"),
            ("slug", "browser-renamed"),
            ("csrfToken", &update_csrf),
            ("idempotencyKey", &update_key),
            ("ifMatch", &update_if_match),
        ])
        .send()
        .expect("browser Project update");
    assert_eq!(updated.status(), 303);
    let updated_location = updated.headers()["location"]
        .to_str()
        .expect("updated detail Location")
        .to_owned();
    assert!(updated_location.contains("/browser-renamed~"));
    let updated_path = url::Url::parse(&updated_location)
        .expect("updated detail URL")
        .path()
        .to_owned();

    let confirmation_page = client
        .get(harness.url(&format!("{updated_path}tombstone/")))
        .send()
        .expect("Project tombstone review");
    assert_eq!(confirmation_page.status(), 200);
    let confirmation_page = confirmation_page.text().expect("tombstone review HTML");
    assert!(confirmation_page.contains("Type the exact Project key"));
    assert!(confirmation_page.contains("0 live Services"));
    assert!(confirmation_page.contains("0 associated Artifacts"));
    let tombstone_csrf = hidden_form_value(&confirmation_page, "csrfToken");
    let tombstone_key = hidden_form_value(&confirmation_page, "idempotencyKey");
    let tombstone_if_match = hidden_form_value(&confirmation_page, "ifMatch");
    let current_key = format!("browser-renamed~{id}");

    let wrong_confirmation = client
        .post(harness.url(&format!("{updated_path}tombstone/")))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://desktop.greyhound-chinstrap.ts.net")
        .header("Sec-Fetch-Site", "same-origin")
        .form(&[
            ("confirmation", "wrong-key"),
            ("csrfToken", &tombstone_csrf),
            ("idempotencyKey", &tombstone_key),
            ("ifMatch", &tombstone_if_match),
        ])
        .send()
        .expect("wrong tombstone confirmation");
    assert_eq!(wrong_confirmation.status(), 403);
    assert!(
        wrong_confirmation
            .text()
            .expect("wrong confirmation HTML")
            .contains("csrf_rejected")
    );

    let tombstoned = client
        .post(harness.url(&format!("{updated_path}tombstone/")))
        .header("Host", "desktop.greyhound-chinstrap.ts.net")
        .header("Origin", "https://desktop.greyhound-chinstrap.ts.net")
        .header("Sec-Fetch-Site", "same-origin")
        .form(&[
            ("confirmation", current_key.as_str()),
            ("csrfToken", &tombstone_csrf),
            ("idempotencyKey", &tombstone_key),
            ("ifMatch", &tombstone_if_match),
        ])
        .send()
        .expect("browser Project tombstone");
    assert_eq!(tombstoned.status(), 303);
    assert_eq!(tombstoned.headers()["location"], updated_location);
    let gone = client
        .get(harness.url(&updated_path))
        .send()
        .expect("gone browser Project");
    assert_eq!(gone.status(), 410);
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
    assert_eq!(status["result"]["policy"]["userVersion"], 5);
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
    let javascript_path = html
        .split("src=\"")
        .find_map(|part| {
            part.strip_prefix("/_static/")
                .and_then(|rest| rest.split('\"').next())
        })
        .map(|rest| format!("/_static/{rest}"))
        .expect("versioned ES module");
    let javascript = reqwest::blocking::get(harness.url(&javascript_path)).expect("ES module");
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
    let javascript_body = javascript.text().expect("ES module body");
    assert!(javascript_body.contains("const body = new URLSearchParams()"));
    assert!(!javascript_body.contains("const body = new FormData(form)"));
    let not_modified = reqwest::blocking::Client::new()
        .get(harness.url(&javascript_path))
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
