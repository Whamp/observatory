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
        Self::start_with(|_| {})
    }

    fn start_with(setup: impl FnOnce(&Path)) -> Self {
        let root = supported_tempdir("temporary root");
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).expect("runtime directory");
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).expect("runtime mode");
        let storage = root.path().join("data");
        setup(&storage);
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
    assert_eq!(status["result"]["policy"]["userVersion"], 3);
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
    let status: Value = serde_json::from_slice(&status.stdout).expect("v3 status JSON");
    assert_eq!(status["result"]["policy"]["userVersion"], 3);

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

    let catalogue =
        rusqlite::Connection::open(harness.storage.join("catalogue.sqlite")).expect("v3 catalogue");
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
            "INSERT INTO artifacts(id,project_id,record_version) VALUES (?1,?2,1)",
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
    assert_eq!(status["result"]["policy"]["userVersion"], 3);
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
