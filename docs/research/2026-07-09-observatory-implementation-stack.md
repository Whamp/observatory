# Observatory implementation stack, packaging, and supervision

Date: 2026-07-09

Ticket: [Choose the implementation stack, packaging, and supervision model](https://github.com/Whamp/observatory/issues/12)

## Decision

Build Observatory as one Linux-native Rust Cargo binary named `obs`:

- Tokio and axum for the loopback HTTP server;
- clap for the approved resource-oriented CLI;
- serde for API and machine-output types;
- rusqlite with the `bundled` and `backup` features, so the release pins upstream SQLite and exposes its Online Backup API;
- an Observatory-owned Linux filesystem deep module over rustix for descriptor-relative traversal, copying, sync, rename, and lock operations; and
- reqwest with default features disabled and its rustls/web-PKI-roots feature for bounded Service reachability probes.

The binary contains the browser shell and its versioned HTML, CSS, and ECMAScript-module assets at compile time. It implements the approved Project-led ledger and keeps fast search secondary. It preserves the `/api/v1/` machine API and the settled route namespaces.

Ship one `x86_64-unknown-linux-gnu` release for the `desktop` deployment first. Run `obs serve` in the foreground under a systemd user service installed only by `obs system setup apply --yes`. Tailscale Serve remains a persistent `tailscaled` configuration, not another foreground process.

This choice is implementation-ready. The remaining proof gates at the end are acceptance tests for the implementation, not reasons to reopen stack planning or create more planning tickets unless a test reveals a major unknown.

## Why Rust

Rust and Go both passed the measured vertical spike. Rust wins because Observatory's hardest boundary is not HTTP throughput: it is a recursive, descriptor-relative, no-follow filesystem protocol with correct descriptor lifetimes through failures and cancellation. Rustix supplies typed owned descriptors, and Rust's ownership and RAII close them reliably on every return path. Rusqlite also bundles the upstream SQLite C implementation directly. The Go alternative uses raw integer descriptors through `x/sys/unix` and adds modernc's translated SQLite/libc stack.

The small startup and memory differences do not decide this choice. Both spikes reached health in milliseconds and charged less than one scheduler tick during each five-second idle sample. Rust's smaller measured artifact and lower idle RSS support the choice, while Go's faster clean build and fewer default threads are genuine advantages. Neither matters as much as typed owned descriptors and upstream bundled SQLite for Observatory's persistence contract.

### Measured Rust-versus-Go spike

The spike compared these pinned shapes on the actual target host:

- Rust: axum 0.8.9, Tokio 1.52.3, rusqlite 0.40.1 with bundled SQLite, rustix 1.1.4, and serde 1.0.228, resolved by `Cargo.lock`.
- Go: Go 1.26.2, `modernc.org/sqlite` 1.53.0, and `golang.org/x/sys` 0.47.0, resolved by `go.mod` and `go.sum`.

Each source built one executable with `serve` and `health` modes. Each executable bound loopback, served embedded HTML and health JSON, created an on-disk SQLite database and `STRICT` table, enabled and verified `foreign_keys=1`, WAL, and `synchronous=FULL`, and reported SQLite 3.53.2. Startup exercised no-follow exclusive creation with a directory-relative descriptor, file sync, same-directory rename, directory sync, cleanup, and a second directory sync. Rust used owned descriptors; Go used raw integer descriptors and explicit closes.

Dependencies were downloaded before build timing. Three serial clean/immediate-incremental build pairs were measured per stack. Runtime startup was measured from process creation to the first valid health JSON over ten fresh processes and fresh databases. Idle CPU was the `/proc/<pid>/stat` tick delta over five seconds after readiness; RSS and thread count came from `/proc/<pid>/status`. The OS page cache was not flushed. Values are median (minimum–maximum).

| Metric | Runs | Rust | Go |
| --- | ---: | ---: | ---: |
| Clean release build | 3 | 41,888.1 ms (41,210.2–43,384.3) | 16,692.5 ms (9,042.4–19,206.2) |
| Immediate incremental build | 3 | 41.24 ms (39.88–49.41) | 49.39 ms (47.72–432.61) |
| Stripped binary size | 1 | 3,471,640 B (3.31 MiB) | 10,615,712 B (10.12 MiB) |
| Fresh process to health | 10 | 2.753 ms (2.553–2.868) | 3.906 ms (3.842–5.502) |
| Idle RSS after 5 seconds | 3 | 5,908 KiB (5,888–5,952) | 14,624 KiB (14,624–14,628) |
| Idle CPU over 5 seconds | 3 | 0 ticks; reported 0.0% | 0 ticks; reported 0.0% |
| Threads after 5 seconds | 3 | 17 | 6 |
| SQLite version | 10 plus smoke | 3.53.2 | 3.53.2 |
| `STRICT` table verification | smoke database | `probe`, `strict=1` | `probe`, `strict=1` |

The Rust binary was a glibc-linked x86-64 PIE with no runtime `libsqlite3` dependency. The Go benchmark used the host defaults, including `CGO_ENABLED=1`, and was also glibc-linked with no runtime `libsqlite3` dependency. Both stripped binaries ran successfully on the target host. The spike measured a minimal server, not the production crash protocol or representative publish load.

### Rejected alternatives

**Go is the second choice, not a parallel implementation.** Its standard HTTP and embedding support, fast clean build, and simple cross-compilation are strong. It loses because the chosen low-level design would manage raw file-descriptor integers manually and depend on modernc's translated SQLite/libc implementation. The mattn SQLite alternative restores upstream SQLite but requires CGo and removes Go's main packaging advantage.

**Node and Bun are rejected as runtimes.** Their standalone executable mechanisms do not expose the complete production-ready `openat2`/descriptor-relative rename and sync surface needed by the settled storage protocol. Adding a native addon or experimental FFI would make the supposed one-file runtime platform-specific and more fragile. Node's single-executable application flow and built-in SQLite also remain coupled to Node release mechanics; Bun couples SQLite to its runtime and adds a more complex distribution-license boundary.

There is no Node, Bun, npm, or frontend runtime in production or in the release build. Observatory's approved ledger needs server-rendered catalogue data, direct links, filters, and secondary client-side search—not client-side routing, offline state, or a large component ecosystem. A SPA framework would duplicate server/API state, add a dependency graph and hydration lifecycle, and weaken direct URL behavior without solving an approved requirement. Plain versioned HTML/CSS/ES modules keep this surface small and accessible. A later UI requirement can justify revisiting the compile-time asset implementation without changing the Rust server or route contract.

## Binary and process model

`obs` is one executable with two roles:

1. Normal CLI invocations parse arguments and call the backend over HTTP. Every catalogue read and write, including diagnostics and recovery, goes through the backend. The CLI never opens SQLite or Artifact storage.
2. `obs serve` is the foreground daemon. It owns SQLite, Artifact storage, scheduled probes and cleanup, and the loopback listener.

The default backend is `http://127.0.0.1:3773`. The existing global `--server URL` behavior remains: it selects the backend for that invocation and has highest client-endpoint precedence. Normal commands never start the daemon, directly or through systemd. A missing backend produces the settled daemon-unavailable failure. Only the explicit setup and service-management path enables supervision.

The daemon takes an exclusive advisory lock at `$XDG_RUNTIME_DIR/observatory/daemon.lock` before opening the catalogue. A second daemon exits nonzero before migration, recovery, background work, or byte writes. The SQLite single-writer rules remain the final data-concurrency guard; the process lock prevents two service owners from competing over startup and scheduled work.

Use a Tokio multithread runtime with four worker threads and at most four blocking threads initially. Put filesystem/SQLite blocking work behind bounded application queues; do not allow unbounded `spawn_blocking` work. Bound probe concurrency independently and apply connect, response-header, overall, and cancellation timeouts. The implementation load gate may reduce these fixed limits when evidence supports it, but it must not return to Tokio's host-CPU-derived defaults.

## HTTP and embedded frontend

Keep the settled namespaces exactly:

- `/` redirects to `/ui/`;
- `/ui/` contains the Project-led ledger and detail/control pages;
- `/api/v1/` is the machine API;
- `/_static/<build-id>/` contains Observatory UI assets;
- `/artifacts/` and `/revisions/` serve catalogue-authorized Artifact bytes; and
- Service Open actions remain direct external URLs, never Observatory proxies.

The build computes `<build-id>` from the complete embedded UI asset set, using a form such as `sha256-<digest>`. Rust compile-time inclusion places the reviewed HTML, CSS, and ES modules in the executable. Requests under the matching `/_static/<build-id>/` receive `Cache-Control: public, max-age=31536000, immutable` and strong content-derived ETags. UI shell responses and `/api/v1/` data receive `Cache-Control: no-store`; they never inherit immutable caching. Artifact cache behavior remains governed by the stable-current versus immutable-Revision contract, not the UI policy.

The server renders the initial Project ledger. Small ES modules provide in-scope fast search, kind filtering, ordering, mobile controls, and progressively enhanced mutation forms. Project navigation and lifecycle/liveness state remain visible when JavaScript is unavailable. No client router owns canonical URLs, and the browser never constructs public resource URLs from IDs.

Expose a lightweight readiness and health representation at `/api/v1/system/health`. It reports the running build, API version, storage health, migration state, startup-recovery state, background-worker state, and Tailscale integration state without exposing paths or secret-bearing data. It becomes reachable only after startup classification and automatic reconciliation finish. A degraded Tailscale state does not make local readiness fail; untrusted catalogue authority reports `offline` and exposes diagnostics but no Entries.

## Configuration and local state

Observatory follows the XDG Base Directory roles. The exact defaults are:

| Purpose | Location |
| --- | --- |
| Configuration file | `${XDG_CONFIG_HOME:-$HOME/.config}/observatory/config.toml` |
| Durable storage root | `${XDG_DATA_HOME:-$HOME/.local/share}/observatory/` |
| Runtime directory | `$XDG_RUNTIME_DIR/observatory/` |
| Daemon lock | `$XDG_RUNTIME_DIR/observatory/daemon.lock` |
| Versioned executables | `$HOME/.local/lib/observatory/versions/<version>/obs` |
| Active executable selector | `$HOME/.local/lib/observatory/current` |
| User command | `$HOME/.local/bin/obs` |
| Generated user unit | `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/observatory.service` |

`XDG_RUNTIME_DIR` must be set, absolute, owned by the user, and mode-protected by the user manager. The daemon creates its runtime subdirectory as `0700`. Durable staging, quarantine, backups, and update versions never go in the runtime directory.

The default durable root has this exact top-level layout:

```text
catalogue.sqlite
staging/<operation-id>/
revisions/<opaque-revision-id>/
quarantine/<operation-or-revision-id>/
backups/<backup-id>/
candidates/<candidate-id>/
```

The whole storage root is private (`0700` directories and no group/other access). `catalogue.sqlite`, its WAL/shared-memory sidecars, staging, Revisions, quarantine, backup staging, and candidates must stay on the same supported local filesystem wherever the settled atomic protocol requires rename. The configured root is rejected for write authority when this condition or the filesystem durability capability check fails.

Configuration precedence is fixed and field-by-field:

1. explicit command-line option;
2. environment variable;
3. `config.toml`;
4. built-in default.

Daemon variables are `OBS_LISTEN`, `OBS_CANONICAL_ORIGIN`, and `OBS_STORAGE`. The client endpoint uses `--server`, then `OBS_SERVER`, then `client.server`, then `http://127.0.0.1:3773`. `obs serve` defaults to `127.0.0.1:3773`; a non-loopback listen address is invalid. The deployment canonical origin defaults to `https://desktop.greyhound-chinstrap.ts.net/`, while setup verifies it against the active node and Serve HTTPS port. Relative XDG or storage paths are invalid.

Configuration is secret-free. It contains no API keys, passwords, cookies, Tailscale auth keys, or updater credentials. Service Targets retain their settled constraint: absolute credential-free HTTP(S) URLs only; no embedded userinfo, bearer tokens, signed query credentials, or fragments used as credentials. The Tailscale grant is the remote authorization boundary, and the installed `tailscale` CLI uses the current local daemon identity. Logs and diagnostics redact the sensitive values already defined by the diagnostics contract.

Configuration does not reload live. Changing `config.toml`, environment, listener, canonical origin, storage root, probe policy, or integration state requires an explicit service restart. `SIGHUP` logs that reload is unsupported and changes nothing. This avoids partially applying storage and authority changes.

## SQLite and filesystem implementation boundaries

Rusqlite compiles a pinned upstream SQLite into `obs`; Observatory does not load the host's `libsqlite3`. The release records the exact SQLite version and compile options in build diagnostics. Every database connection receives and verifies `foreign_keys=ON`, WAL, `synchronous=FULL`, the bounded busy policy, application ID, and expected schema before entering its pool. Connection creation fails rather than returning an incompletely configured connection.

Put all Linux byte-tree operations behind one deep module whose public operations express protocols, not generic path manipulation: validate/copy source tree into owned staging, durably finalize Revision, quarantine Revision, durably activate candidate, and remove owned quarantined tree. Internally it uses rustix owned descriptors, `openat2` resolution restrictions on the supported kernel, no-follow traversal, inode-type and mount-boundary checks, same-filesystem `renameat`/`renameat2`, and file and parent-directory `fsync`. It rejects symlinks, FIFOs, sockets, devices, unexpected mount crossings, and source mutation rather than following or guessing.

Use reqwest with default features disabled and `rustls-tls-webpki-roots` for Service probes. Do not compile native-TLS/OpenSSL features. Probes follow the settled reachability contract, never proxy or expose a Service, and never treat a response status as application health.

## Release, package, and activation

Pin the Rust toolchain used to release, commit `Cargo.lock`, and build with `cargo build --release --locked --target x86_64-unknown-linux-gnu`. The first supported release target is the measured x86-64 Arch desktop: kernel 7.0.14, glibc 2.43, Rust 1.96.1, systemd 261, Tailscale 1.98.8, 60 GiB RAM, and local btrfs home storage with 237 GiB available. Build and test against an explicit glibc baseline no newer than 2.43. Musl, other architectures, other Linux distributions, macOS, and Windows are unsupported until their complete SQLite, DNS, rustls, filesystem, service-manager, and ABI test matrices pass. Do not describe an untested musl build as portable.

A release contains:

- the stripped `obs` binary and unstripped debug symbols as separate artifacts;
- SHA-256 checksums;
- signed build provenance binding source commit, pinned toolchain, target, lockfile, and build command;
- an SPDX or CycloneDX SBOM;
- the complete third-party license inventory and notices; and
- passing format, Clippy, tests, dependency-policy, vulnerability, and `cargo-deny` license checks.

The initial package is this verified release bundle plus its explicit user installer; no Arch, Debian, RPM, container, Node, or language-runtime package is claimed. Installation stages the binary under `$HOME/.local/lib/observatory/versions/.staging-<version>/`, verifies checksum and provenance, syncs the file and directory, renames it to `versions/<version>/`, and syncs `versions/`. Activation creates a temporary relative `current` symlink and atomically renames it over `$HOME/.local/lib/observatory/current`, then syncs the parent. `$HOME/.local/bin/obs` is a stable symlink through `current`. The systemd unit executes `$HOME/.local/lib/observatory/current/obs serve`.

Keep the previous executable version until the new version passes process, API-version, build-version, migration, storage-health, and loopback request checks. Garbage-collect older executable versions only after a later explicit successful activation. Updating the binary never edits the database directly.

## Migration, update, and rollback

Startup compares the binary's supported schema range with `application_id` and `user_version` under the exclusive daemon/migration lock.

- A matching schema proceeds to recovery.
- A newer unknown schema enters diagnostic-only `offline` mode and performs no catalogue or byte mutation.
- An older supported schema triggers ordered, transactional, monotonic migrations.
- An invalid application ID or corrupt catalogue enters diagnostic-only `offline` mode.

Before the first migration, the daemon must pass the settled backup gate: acquire a backup lease for the exact committed Revision set, use rusqlite's Online Backup API, bind the snapshot to immutable Revision bytes, finish and verify the backup, and record its identity. A database-file copy is not a backup. Failure to create or verify the backup aborts migration. Each migration transaction updates `user_version`, then the new binary runs foreign-key and quick checks before writes are enabled.

The update sequence is:

1. verify and durably stage the new version;
2. preserve the old active selector and inspect the new binary's schema compatibility metadata;
3. atomically activate the new executable;
4. restart `observatory.service`;
5. wait with a bounded timeout for the exact new build at `/api/v1/system/health`;
6. require startup reconciliation to finish and health to permit the expected write classes; and
7. retain the prior executable and any pre-migration backup after success.

Failure before a migration commits permits automatic executable-selector rollback and another restart. After a migration commits, automatic rollback to the previous binary is allowed only when that binary declares support for the resulting schema. Otherwise the service stays on the new binary in its reported healthy, degraded, unhealthy, or offline state; it does not run old code against a newer schema. Restoring pre-migration data is a separate offline, previewed, confirmed recovery operation under the settled issue #16 contract. It can discard changes made after the backup, so neither the updater nor systemd performs it automatically.

## systemd user supervision

`obs system setup check` is read-only. It validates the executable paths, config, loopback bind, storage filesystem and permissions, available capacity, user-manager state, linger, unit drift, installed Tailscale version/state, canonical origin, and the configured root Serve handler.

Only `obs system setup apply --yes` writes setup state. It installs or updates the generated unit, runs `systemctl --user daemon-reload`, explicitly enables and starts the service, verifies loopback health, and only then configures the one owned Serve root handler after conflict checks. It verifies `Linger=yes`; missing linger is a failed precondition with the exact operator command needed to enable it, not permission for `obs` to invoke privileged `loginctl` or sudo. On this host linger is already enabled. The current user manager is degraded only because unrelated `firepass-monitor.service` is missing; setup reports that unrelated failed unit but does not attribute it to Observatory or block an otherwise healthy Observatory unit.

Choose `Type=exec`. Observatory does not implement the systemd notification protocol merely to claim `Type=notify`. Because `Type=exec` reports successful `execve`, not application readiness, the daemon binds only after configuration, authority classification, migration, backup gating, and automatic startup reconciliation. The loopback health endpoint is the readiness authority for setup and updates.

The generated unit has this policy:

```ini
[Unit]
Description=Observatory catalogue and Artifact server
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=exec
ExecStart=%h/.local/lib/observatory/current/obs serve
Restart=on-failure
RestartSec=5s
TimeoutStopSec=30s
KillSignal=SIGTERM
UMask=0077
NoNewPrivileges=yes
ProtectSystem=full
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes

[Install]
WantedBy=default.target
```

The unit deliberately omits `ProtectHome`, `PrivateTmp`, a syscall filter, `DynamicUser`, and a restricted working directory. The daemon must write its private data, read arbitrary operator-selected Publish sources, and execute an approved Project Teardown Action in that Project's canonical directory. Those broader sandboxes, and `ProtectSystem=strict`, would silently break settled capabilities. `ProtectSystem=full` makes `/usr`, the boot directories, and `/etc` read-only without blocking home Projects; `NoNewPrivileges` prevents a Teardown Action from gaining privilege. The unit contains no secrets and does not depend on `tailscaled` or network-online startup: local catalogue availability must not wait for the tailnet.

SIGTERM stops new requests and background dispatch, lets in-flight short catalogue transactions finish, cancels probes, records resumable intents for interrupted long byte operations, checkpoints WAL only when safe and bounded, closes the listener and database, and exits within 30 seconds. After that systemd may send SIGKILL; startup recovery must handle every resulting boundary. A requested clean stop exits zero and does not restart. A crash or nonzero fatal exit restarts after five seconds, with at most five starts per 60 seconds. Reaching the limit leaves the unit failed for operator action rather than looping indefinitely.

Startup follows this order:

1. parse and validate secret-free configuration;
2. create the private runtime directory and acquire the daemon lock;
3. open/classify storage and configure every SQLite connection;
4. validate application/schema identity and run backup-gated migration when required;
5. reconcile recorded nonterminal Publish, cleanup, backup, and recovery intents;
6. classify write and serving gates exactly as settled by issue #16;
7. bind loopback and expose health, diagnostics, permitted catalogue reads, and permitted writes;
8. start bounded cleanup and probe workers; and
9. read and report Tailscale Serve state without mutating it.

A malformed config, non-loopback bind request, unavailable runtime directory, held daemon lock, or listener bind failure exits nonzero. Catalogue corruption, wrong/new schema, unsupported storage, ambiguous recovery, capacity pressure, or damaged Revision bytes starts the backend in the precise diagnostic-only or partially degraded mode defined by issue #16: untrusted authority exposes no Entries; affected Revisions become unavailable; interrupted operations block byte-adding writes; capacity pressure blocks byte-adding writes but preserves intact reads and cleanup. Tailscale failure changes only integration health. It never makes local storage startup fatal.

Supervision failures are visible through `systemctl --user status observatory.service`, `systemctl --user show observatory.service`, and `journalctl --user-unit observatory.service`. `obs` writes structured operational logs to stderr; systemd sends them to journald. It does not create a second log-file lifecycle under XDG data. Logs include build ID, startup phase, exit category, schema/migration state, recovery operation IDs, listener state, and Tailscale check category, while applying the settled redaction rules.

## Tailscale Serve ownership

The canonical external origin remains `https://desktop.greyhound-chinstrap.ts.net/`. Its HTTPS root handler proxies to `http://127.0.0.1:3773`; the local port is independently configurable and never appears in public URLs. The backend never binds a tailnet or LAN address.

Setup owns only the root handler for the configured canonical host and HTTPS port. `obs system setup check` uses the installed `tailscale` CLI, including `tailscale serve status --json`, to compare live state with the desired root proxy. Apply re-reads state immediately before mutation, refuses an unrelated or conflicting root handler, invokes the installed CLI noninteractively to persist that one root proxy, and verifies the exact post-state. It preserves unrelated Serve handlers. Teardown may remove only a handler whose recorded and live fingerprints still prove Observatory ownership.

Ordinary daemon startup and periodic diagnostics are read-only toward Serve. They may invoke status commands but never `serve`, `set-config`, `reset`, `clear`, or `drain`. A stopped or logged-out tailscaled, missing CLI, grant/DNS/certificate problem, or tailnet outage reports a degraded integration while loopback CLI/API and valid local storage continue. The daemon applies bounded backoff to status checks and does not restart merely because Serve is unavailable.

There is no foreground `tailscale serve` child. Persistent Serve configuration belongs to tailscaled. Observatory never adds a handler for a Service Target, never proxies an external Service, and never changes that Service's exposure, grant, process, port, or TLS policy.

## Implementation acceptance criteria

The production implementation must pass all six remaining gates from the measured spike:

1. **Adversarial walker races.** Exercise recursive descriptor-relative traversal against symlink swaps, renamed parents/children, mount crossings, hard-to-classify inode types, and concurrent source changes. Prove no path escape, link following, unsafe inode copy, descriptor leak, or unintended overwrite.
2. **Crash and fault injection.** Kill or fail the process after every database transaction, file/directory sync, rename, WAL, migration, backup, cleanup, candidate activation, and shutdown boundary. Inject partial writes, I/O errors, full disk, busy locks, and checkpoint failures. Prove restart reaches only a settled valid state or the specified diagnostic gate.
3. **Every-connection SQLite policy.** Exercise creation, reuse, failure, and replacement of every live connection. Prove each connection has the pinned engine, application/schema identity, foreign keys, WAL, `synchronous=FULL`, and bounded busy behavior before use.
4. **Tokio thread policy and load.** Test the explicit four-worker/four-blocking-thread limits and bounded probe/publish queues under concurrent ledger reads, Artifact streaming, publishes, cleanup, probes, cancellation, and shutdown. Record idle and loaded CPU/RSS/thread/latency behavior and reduce limits when the target evidence supports it.
5. **systemd, package, and Serve integration.** On the real user manager, test explicit setup, linger detection, enable/start/stop, SIGTERM and SIGKILL recovery, restart limiting, journald diagnostics, version activation, health verification, migration backup gating, compatible/incompatible rollback, unrelated Serve handler preservation, root conflict refusal, tailnet outage, and current Tailscale 1.98.8 behavior.
6. **Target ABI and static options.** Test the released glibc artifact on the declared x86-64 baseline and actual desktop, inspect dynamic linkage, and prove bundled SQLite and rustls behavior. Treat musl/static and every other platform as unsupported until equivalent filesystem, SQLite, DNS/TLS, systemd/package, and Serve tests pass.

A failed gate is implementation work within this decision. Open a new planning decision only when the failure reveals a major requirement or architecture unknown that cannot be resolved without changing this contract.

## Primary sources and settled inputs

- [Observatory map and scope](https://github.com/Whamp/observatory/issues/1)
- [Canonical address and tailnet trust boundary](https://github.com/Whamp/observatory/issues/5)
- [Routes and namespaces](https://github.com/Whamp/observatory/issues/6)
- [Agent-facing CLI contract](https://github.com/Whamp/observatory/issues/9)
- [Approved Project-led index](https://github.com/Whamp/observatory/issues/10#issuecomment-4931105631)
- [Persistence and indexing architecture](./2026-07-09-observatory-persistence-architecture.md)
- [Storage diagnostics and recovery](./2026-07-09-observatory-storage-diagnostics-recovery.md)
- [axum documentation](https://docs.rs/axum/0.8.9/axum/)
- [Tokio runtime builder](https://docs.rs/tokio/1.52.0/tokio/runtime/struct.Builder.html)
- [clap documentation](https://docs.rs/clap/)
- [serde documentation](https://serde.rs/)
- [reqwest feature documentation](https://docs.rs/reqwest/latest/reqwest/#optional-features)
- [rusqlite bundled SQLite and backup features](https://github.com/rusqlite/rusqlite)
- [rustix filesystem API](https://docs.rs/rustix/latest/rustix/fs/)
- [Cargo locked builds and target selection](https://doc.rust-lang.org/cargo/commands/cargo-build.html)
- [SQLite Online Backup API](https://sqlite.org/backup.html), [WAL](https://sqlite.org/wal.html), and [PRAGMAs](https://sqlite.org/pragma.html)
- [Linux `openat2(2)`](https://man7.org/linux/man-pages/man2/openat2.2.html), [`rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html), and [`fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/)
- [systemd service unit manual](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html) and [execution environment manual](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)
- [Tailscale Serve CLI](https://tailscale.com/docs/reference/tailscale-cli/serve) and [Serve behavior](https://tailscale.com/docs/features/tailscale-serve)
- [Go `embed`](https://pkg.go.dev/embed), [`net/http`](https://pkg.go.dev/net/http), and [`x/sys/unix`](https://pkg.go.dev/golang.org/x/sys/unix)
- [modernc SQLite package](https://pkg.go.dev/modernc.org/sqlite)
- [Node single-executable applications](https://nodejs.org/api/single-executable-applications.html) and [Node SQLite](https://nodejs.org/api/sqlite.html)
- [Bun standalone executables](https://bun.com/docs/bundler/executables), [Bun SQLite](https://bun.com/docs/runtime/sqlite), and [Bun FFI status](https://bun.com/docs/runtime/ffi)
