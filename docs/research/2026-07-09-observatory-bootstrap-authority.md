# Observatory bootstrap and setup authority

Date: 2026-07-09

Ticket: [Define first-install bootstrap and setup authority](https://github.com/Whamp/observatory/issues/19)

## Decision

The invoking release-bundle `obs` process is Observatory's sole **local bootstrap authority**. Its authority is narrow: it may verify and install a release, activate executable versions, create secret-free configuration, install and control the generated systemd user unit, record setup receipts, and manage one receipt-bound Tailscale Serve root handler.

The `obs serve` daemon remains the sole **domain authority**. It alone opens SQLite, creates or inspects the authoritative data root, manages the Revision store, performs catalogue migrations and reconciliation, runs diagnostics and recovery, creates backups, cleans up data, and serves the API and UI.

These authorities do not overlap. Bootstrap exists only because a fresh machine has no daemon to receive an install request. It does not create a second path to Observatory data.

## Fixed public surface

The only public commands that use local bootstrap authority without a daemon are:

```text
obs system setup check
obs system setup apply --yes
obs system setup remove --yes
obs system setup uninstall --yes
obs system service status
obs system service start
obs system service stop --yes
obs system service restart --yes
```

This inventory is fixed by the [public control-plane decision](https://github.com/Whamp/observatory/blob/master/docs/research/2026-07-09-observatory-public-control-plane.md). Service lifecycle verbs belong under `system service`, not `system setup`. No other local bootstrap leaf is public.

`--server` selects a daemon for daemon-backed commands. Supplying it to any local setup or service leaf is a usage error; bootstrap never redirects local machine management to another host.

There is no `/api/v1` endpoint for release installation, executable activation, setup removal, uninstall, systemd mutation, or Tailscale Serve mutation. Local bootstrap may make bounded loopback requests only to `/api/v1/system/health`. It does not call other daemon APIs.

Every normal command remains daemon-only. This includes `system status`, configuration inspection and validation, diagnostics, recovery, backup, cleanup, and every Project, Artifact, Service, and Target operation. Those commands never acquire the setup lock, inspect or invoke systemd, open local storage, or autostart the daemon. A missing daemon returns the settled `daemon_unavailable` error and exit `5`.

## Capability boundary

Implement local bootstrap behind an explicit capability allowlist. The adapter may use only:

- release-bundle verification and host compatibility checks;
- private filesystem operations for installation, configuration, unit files, locks, and receipts;
- fixed `systemctl --user` lifecycle operations and machine-readable unit/process inspection;
- fixed, noninteractive Tailscale Serve status and handler-specific operations;
- bounded loopback requests to `/api/v1/system/health` and separate canonical HTTPS Serve verification; and
- redacted process, socket, and journal metadata needed to classify startup failures.

The adapter cannot import, link, or call:

- the SQLite connection factory or catalogue repository;
- Revision, staging, quarantine, backup, candidate, or recovery storage modules;
- the domain command dispatcher or domain mutation services; or
- any helper that discovers authority from catalogue or storage contents.

Architecture tests must enforce this dependency direction. The local adapter must not open or inspect `catalogue.sqlite`, its WAL/SHM files, Revisions, backup contents, candidates, staging, or quarantine. A receipt proves ownership of deployment state only; it cannot create an Entry or establish catalogue truth.

The daemon owns all catalogue and storage migrations. Bootstrap may inspect static release compatibility metadata and observe migration state through health, but it never runs a migration, restores data, changes catalogue candidates, or infers health from files.

## Command semantics

| Command | Exact effect |
| --- | --- |
| `system setup check` | Read-only prerequisite, conflict, compatibility, and exact-change preview for the invoking release candidate. Creates nothing. |
| `system setup apply --yes` | Confirmed first install, repair of owned deployment integration, or upgrade. Verifies and stages the candidate, activates it, installs the unit, starts or restarts the daemon, proves exact health, then configures the owned Serve root. |
| `system setup remove --yes` | Removes only the owned Serve root handler and the matching generated user unit after stopping and disabling it. Retains installed versions, `current`, stable command, configuration, and all data. |
| `system setup uninstall --yes` | Performs remove, then deletes only verified owned stable-command, `current`, version, and setup-receipt state. Retains configuration and the complete authoritative data root. |
| `system service status` | Reads installation, unit, process, loopback health, and Serve ownership state. It does not start or install anything. |
| `system service start` | Starts only an installed, owned Observatory unit and verifies exact build/API health. It does not install or enable a deliberately disabled unit. |
| `system service stop --yes` | Stops only the owned unit and verifies inactivity. It leaves enablement, Serve configuration, configuration, executables, and data unchanged. |
| `system service restart --yes` | Restarts only the owned unit and verifies exact build/API health. |

There is no local data-purge option. Removing authoritative storage outside the daemon would violate the authority boundary. A future purge, if approved, must be a separate daemon-backed, previewed recovery operation.

## Release trust and first installation

### External trust comes first

A standalone executable cannot prove its own authenticity: an attacker who substitutes the executable can also substitute its embedded verifier. First installation therefore starts with external verification of a complete Observatory release bundle against the published Observatory release identity or a pinned public key.

The bundle contains:

- the stripped `obs` executable;
- a SHA-256 manifest;
- a detached signature;
- signed provenance binding the source commit, Cargo lockfile, Rust toolchain, target, and build command;
- the SBOM and license inventory/notices; and
- separate symbols.

Before running `obs`, the operator verifies the bundle checksum, signature, and provenance with tooling outside that bundle. The candidate then repeats bundle consistency, digest, signature, provenance-subject, target, and version checks. Candidate self-verification is defense in depth, not the first-install trust anchor. `setup apply` rejects a bare executable or incomplete release metadata.

The running candidate must be the regular file named by the verified manifest. Reject a symlink, a non-regular file, an unexpected hard link, a digest mismatch, an unsupported target, a provenance mismatch, or a version inconsistent with provenance.

The first-install journey is exact:

```text
1. Download and unpack one complete Observatory release bundle.
2. Externally verify its SHA-256 manifest, signature, and provenance.
3. Run ./obs system setup check --json.
4. Resolve every failed precondition.
5. Run ./obs system setup apply --yes --idempotency-key <unique-key>.
6. Use ~/.local/bin/obs thereafter.
```

Bootstrap does not choose a package manager, download an unverified replacement, use `sudo`, or install a system-level service.

## Version staging and activation

Installation state has this shape:

```text
$HOME/.local/lib/observatory/
  versions/
    .staging-<version>-<operation-id>/
      obs
      release.json
    <version>/
      obs
      release.json
  current -> versions/<version>
  install-state.json
$HOME/.local/bin/obs -> ../lib/observatory/current/obs
```

`setup apply` activates a release in this order:

1. Validate the user boundary, `$HOME`, destination ownership and permissions, target architecture, and glibc baseline.
2. Copy verified release members into a unique staging directory.
3. Re-hash the staged executable and recheck provenance.
4. Set private permissions and reject group/world-writable state.
5. Sync staged files and the staging directory.
6. Atomically rename staging to `versions/<version>/` and sync `versions/`.
7. Create a temporary relative `current` symlink and atomically rename it over `current`.
8. Sync `$HOME/.local/lib/observatory/`.
9. Create or atomically replace the relative `$HOME/.local/bin/obs` symlink.
10. Resolve both links and verify the selected executable's digest.

An existing version directory with the same verified digest is reusable. The same version with different bytes is an `ownership_conflict` and is never overwritten.

The generated unit always executes the selector, not a version-specific path:

```ini
ExecStart=%h/.local/lib/observatory/current/obs serve
```

The prior active version survives health verification and at least one later successful activation. Setup never removes the only rollback-capable previous version.

## XDG ownership and creation boundaries

Observatory follows the [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/). Configured XDG paths must be absolute. `$XDG_RUNTIME_DIR` must be user-owned, mode `0700`, local, and valid for the login lifetime; a missing or invalid runtime directory is a hard precondition rather than a silently reinterpreted path.

| Path | Creator and authority |
| --- | --- |
| `${XDG_CONFIG_HOME:-$HOME/.config}/observatory/` | `setup apply`; private local configuration directory |
| `config.toml` | `setup apply` when absent, or a previewed atomic config migration; secret-free |
| `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/observatory.service` | `setup apply`; exact generated user unit |
| `${XDG_DATA_HOME:-$HOME/.local/share}/observatory/` | Daemon only; complete authoritative data boundary |
| `$XDG_RUNTIME_DIR/observatory/` | First setup mutation or daemon startup; mode `0700` |
| `setup.lock` | Setup mutation; serializes bootstrap effects |
| `daemon.lock` | Daemon; prevents multiple domain authorities |
| `$HOME/.local/lib/observatory/` | `setup apply`; releases and non-authoritative install receipt |
| `$HOME/.local/bin/obs` | `setup apply`; stable command symlink |

`setup check` creates no directory, file, lock, receipt, configuration, unit, selector, executable, or data path. It may inspect existing paths and parent filesystem capacity without writing a probe.

`setup apply` may create configuration, runtime coordination, installation, selector, receipt, and generated-unit state. Starting the daemon may then cause the daemon to create the authoritative data root, catalogue, and storage layout. Those data effects belong to daemon startup, never to the setup process.

## Setup lock, idempotency, and receipts

### Locking

Every mutating local leaf:

1. validates `$XDG_RUNTIME_DIR`;
2. creates `$XDG_RUNTIME_DIR/observatory/` as mode `0700` when absent;
3. obtains an exclusive advisory lock on `setup.lock`;
4. waits no longer than five seconds; and
5. returns retryable `contention` without mutation when the bound expires.

`setup check` and `service status` are lock-free snapshots. They report `snapshotStable:false` when they observe an active setup operation. Setup never acquires the daemon lock and never crosses into storage while holding its own lock.

### Idempotency rules

Read-only `setup check` and `service status` require no idempotency key.

All API mutations and all non-TTY automation mutations require a caller-supplied idempotency key under the public control-plane contract. There is no setup/service-manager mutation API, but non-TTY use of a mutating local leaf follows the same rule. A local TTY mutation may omit the key; Observatory generates one and displays it in the result so the operator can replay safely.

A receipt binds the key to a canonical request fingerprint. Reusing the same key and fingerprint returns or resumes the recorded operation. Reusing the key with changed release, configuration, or requested effect returns `conflict`. Completed effects are re-verified rather than blindly repeated.

### Setup receipt

`install-state.json` records non-secret deployment facts:

- install ID, operation ID, idempotency key, and request fingerprint;
- current and previous release versions and digests;
- generated-unit digest and prior enablement/activity state;
- config digest and schema version;
- expected loopback listener and canonical origin;
- owned Serve handler tuple and fingerprint;
- completed phase, terminal result, and partial result;
- rollback eligibility and retained previous release.

Receipt writes use a temporary file, file sync, atomic rename, and parent-directory sync. A receipt is evidence for setup ownership and crash recovery only. It has no authority over catalogue state.

## Exact `setup check` flow

`setup check` is a deterministic read-only preview of the invoking candidate. It reports ordered checks and proposed actions (`create`, `replace`, `retain`, `restart`, `configure`, or `blocked`) for:

1. complete bundle digest, external-signature/provenance evidence, candidate self-check, and target support;
2. non-root execution, matching real/effective UID, home ownership, and absence of a sudo-derived target user;
3. absolute XDG paths and protected local runtime directory;
4. destination ownership, permissions, filesystem support, and prospective free capacity;
5. installed versions, `current`, stable command, and receipt state;
6. candidate and previous release schema compatibility metadata;
7. config parsing, precedence, loopback listener, absolute storage path, and previewed config migration;
8. prospective storage parent/filesystem support without opening or creating storage;
9. setup-lock occupancy;
10. systemd user-manager availability;
11. linger state;
12. generated-unit presence, exact digest, drift, enablement, and active state;
13. MainPID/cgroup/executable state and last unit failure metadata;
14. loopback port occupancy and daemon health identity;
15. Tailscale CLI compatibility, daemon login/state, node DNS name, HTTPS port, and Funnel exclusion;
16. live `tailscale serve status --json` classification;
17. desired root absence, exact receipt-bound ownership, matching-unowned state, or conflict;
18. canonical-origin host and port agreement; and
19. the explicit tailnet-grant requirement and the fact that remote authorization still needs external verification.

Apply never trusts a prior preview as a durable plan. It re-reads and fingerprints all state while holding the setup lock.

Missing linger blocks apply and produces an argv-safe operator action such as:

```text
sudo loginctl enable-linger <user>
```

Observatory reports that action but never runs `sudo`, `loginctl enable-linger`, Polkit elevation, or another privileged mutation. Linger keeps a user's manager available after logout and remains an administrator-controlled setting; see [`loginctl`](https://www.freedesktop.org/software/systemd/man/latest/loginctl.html).

Unrelated failed user units produce warnings, not setup failures.

## Exact `setup apply` flow

`setup apply --yes` executes these phases:

1. **Lock and replay.** Acquire `setup.lock`; return a verified terminal replay when the key and request match.
2. **Recheck.** Repeat candidate, path, config, user-manager, linger, unit, process, listener, and Serve checks under the lock.
3. **Refuse conflicts.** Make no writes when a nominally owned path, unit, listener, daemon, or Serve root is foreign or ambiguously owned.
4. **Stage release.** Durably install the candidate version without activating it.
5. **Prepare rollback.** Record the prior selector, unit, config, enablement, active state, and compatibility metadata.
6. **Write config.** Create defaults only when absent or perform exactly the previewed atomic migration.
7. **Install unit.** Atomically install the exact generated unit and run `systemctl --user daemon-reload`.
8. **Enable.** Explicitly enable `observatory.service`.
9. **Activate.** Atomically select the candidate through `current`.
10. **Transition daemon.** Start or restart only the proven owned Observatory unit as required.
11. **Verify local health.** Poll loopback health with a bound and require the exact candidate build and API.
12. **Mutate Serve root.** Re-read live Serve state and add only the desired root handler.
13. **Verify exposure.** Re-read Serve JSON and request the canonical HTTPS front door from the host.
14. **Commit receipt.** Record the selected release, unit, health, Serve fingerprint, result, and rollback state.

Systemd enablement and activation are separate. Setup runs both operations explicitly; it never assumes enablement started the unit. `Type=exec` proves `execve`, not application readiness, so only Observatory health can complete deployment. See [`systemctl`](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html) and [`systemd.service`](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html).

### Exact health gate

The bounded loopback `/api/v1/system/health` gate requires:

- HTTP success at the configured loopback endpoint;
- expected install/build ID and API version;
- the configured listener identity;
- startup classification and reconciliation completion;
- terminal migration state;
- a reported storage health of `healthy`, `degraded`, `unhealthy`, or `offline`;
- proof that the response came from the candidate rather than a stale or foreign daemon.

No other loopback daemon endpoint is queried. Canonical HTTPS front-door verification occurs separately after the handler-specific Serve phase.

A permitted storage or Tailscale-only degradation may pass local readiness with an explicit warning. `unhealthy` or `offline` proves a completed daemon response but fails setup's healthy-deployment objective. Setup preserves the diagnostic daemon and directs the operator to daemon-backed status, diagnostics, or recovery rather than inspecting data itself.

### Commit and partial-result boundaries

Before selector activation, failure removes only owned incomplete staging and restores setup-owned file changes. After activation but before health, setup attempts executable, unit, and config rollback only when compatibility rules permit it.

Successful local health commits the local installation. A later Tailscale outage does not roll back a healthy daemon: apply returns a trustworthy partial result, exit `8`, records `serve=pending`, and allows the same request to resume that phase.

A pre-existing unrelated Serve root conflict is found before local mutation and blocks all apply effects. If the root changes after local health but before Serve mutation, the healthy local installation remains committed; setup returns a partial conflict without overwriting the root.

On a first-install start failure, setup disables and removes the newly generated unit and restores prior owned command/selectors when present. It retains any data the daemon created as evidence. Setup never deletes authoritative state.

## Existing, old, partial, and foreign daemons

Setup classifies unit ownership/digest, MainPID/cgroup, `/proc/<pid>/exe`, loopback socket ownership, health build/API, and daemon-lock/process evidence independently.

| Observed state | Required handling |
| --- | --- |
| No daemon or unit | Normal first install. |
| Owned healthy daemon, same build | Fully verify and return unchanged idempotent success. |
| Owned healthy daemon, older build | Stage, activate, restart, and verify the candidate. |
| Owned inactive unit | Reconcile owned unit/enablement as requested and start. |
| Owned failed unit | Report last result and bounded journal guidance; apply or explicit service start may reset Observatory's failed state once and start once. |
| Owned old daemon with unhealthy/offline storage | Upgrade only when release compatibility permits; never repair storage locally; preserve diagnostic access. |
| Unit active but health absent | Inspect PID, executable, cgroup, and socket; stop only when all evidence proves the process belongs to the owned unit. |
| Daemon lock held by an unmanaged process | Refuse; do not kill or signal it. |
| Port held by a foreign process or wrong health identity | Return `ownership_conflict`; do not stop, replace, proxy, or signal it. |
| Drifted generated unit | Report the digest category; replace only with confirmed apply and proven Observatory ownership. A foreign same-name unit is a conflict. |
| Start limit reached | Reset only Observatory's failed state/rate counter once, then attempt one start. Persistent failure remains visible for diagnosis. |

[`systemctl reset-failed`](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html#reset-failed%20%5BPATTERN%E2%80%A6%5D) also resets start-rate counters. Bootstrap therefore bounds reset and retry rather than creating a restart loop.

## systemd user-service lifecycle

Install this fixed unit:

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

Lifecycle rules are exact:

- Apply reloads the user manager, enables the unit, starts or restarts as needed, and then checks health.
- Service start requires an installed owned unit, starts it, and checks health. It does not install or implicitly enable it.
- Service stop performs a clean `systemctl --user stop`, then verifies inactivity. Persistent Serve may temporarily report an upstream failure; service status states that plainly.
- Service restart restarts the unit and then verifies the exact selected build and API.
- Setup remove preflights all ownership, removes the owned Serve root, stops and disables the unit, removes only the matching generated unit, and reloads the user manager.
- Setup uninstall performs remove before deleting verified installation state.

Use `systemctl show --property=...` for machine parsing rather than scraping human `status` output. The unit writes stderr to journald. Human guidance may point to:

```text
systemctl --user show observatory.service
systemctl --user status observatory.service
journalctl --user-unit observatory.service
```

Any journal excerpt embedded in machine output is bounded and redacted.

## Tailscale Serve ownership

### Owned tuple

Observatory owns at most this one handler:

```text
canonical MagicDNS host
+ configured HTTPS port
+ root path "/"
+ reverse-proxy target http://127.0.0.1:<configured-port>
```

It does not own non-root mounts, handlers on other ports, Service Target handlers, Funnel, tailnet grants, HTTPS policy, node login, DNS names, or certificate policy. Tailscale Serve terminates HTTPS and proxies to the loopback backend; tailnet grants remain the authorization boundary. See the primary [Serve CLI](https://tailscale.com/docs/reference/tailscale-cli/serve), [Serve behavior](https://tailscale.com/docs/features/tailscale-serve), and [grants](https://tailscale.com/docs/reference/syntax/grants) documentation.

### Classification and mutation

From `tailscale serve status --json`, bootstrap classifies the root handler as:

- `absent`;
- `owned_exact`, when the live tuple exactly matches the handler and install ID recorded by Observatory;
- `matching_unowned`, when the desired target exists without Observatory's ownership receipt;
- `conflicting`, when another mode, target, or handler owns root; or
- `unknown`, when status is unavailable or unsupported.

Only `absent` and `owned_exact` are mutable states. A matching but unowned handler is never adopted. This prevents a later remove from deleting another operator's configuration.

Apply checks Serve state before local mutation, rechecks immediately before its one handler mutation, uses a noninteractive persistent root-specific operation, verifies the exact resulting tuple, and proves unrelated handlers stayed unchanged. It never invokes `tailscale serve reset`, a whole-configuration replacement, or an all-handler adoption flow.

Remove and uninstall delete the root handler only when both receipt and live tuple match. An already absent formerly owned handler is idempotently removed. A drifted root blocks teardown of the handler and unit; bootstrap preserves all unrelated Serve state. Handler removal uses the handler-specific original flags, then re-reads JSON and proves unrelated mounts remained unchanged.

Setup requires Serve HTTPS prerequisites to exist before apply so it never enters an interactive consent flow. It cannot enable Funnel or mutate tailnet policy.

### Grants and canonical verification

The operator must install an explicit least-privilege tailnet grant for intended humans and agent devices to the desktop node's configured HTTPS port. Host-local checks cannot prove effective remote policy.

Apply verifies node and Serve state locally and requests the canonical HTTPS origin from the host. Acceptance also requires:

- success from an intended remote tailnet client;
- denial from an ungranted tailnet identity or device;
- denial from LAN-only and public clients;
- no remotely reachable backend loopback port; and
- no Funnel listener on the Observatory port.

## Upgrade, migration, and rollback

### Configuration migration

Configuration is local setup state and remains secret-free. Each format has a schema version.

The candidate parses the current configuration without modifying it. Check previews every field change and restart effect. Apply writes and syncs a versioned backup, writes the migrated configuration to a temporary file, validates it, atomically replaces `config.toml`, and syncs the parent directory. Unknown or ambiguous fields fail closed; setup never silently drops them. A configuration already valid for the candidate is not rewritten.

Remove and uninstall retain configuration.

### Catalogue migration

Only the candidate daemon may migrate the catalogue. It obtains the daemon lock, creates and verifies the required complete backup, and follows the settled storage migration gates. Setup observes migration through health. It never opens SQLite, creates a backup, selects a recovery candidate, or restores data.

### Executable rollback

Before activation, setup reads static compatibility metadata from the candidate and prior release. Automatic executable rollback is permitted only when:

- failure happened before the candidate could commit a catalogue migration; or
- the prior release explicitly supports the candidate's resulting schema range.

A process may commit a migration immediately before failing. When the resulting schema is uncertain and backward compatibility is not declared, bootstrap does not select the old executable automatically.

For an incompatible or uncertain post-migration failure, setup:

- keeps the candidate selected;
- keeps the prior executable and pre-migration daemon backup;
- leaves the unit failed or diagnostic-only;
- reports `automaticRollbackAllowed:false`; and
- directs the operator to daemon-backed status, diagnostics, and previewed recovery.

Setup never rolls back data. Backup restoration remains an offline, daemon-backed, previewed, confirmed recovery operation.

### Crash and retry outcomes

| Interruption or retry | Result |
| --- | --- |
| Repeated apply against exact healthy state | `changed:false` after full verification. |
| Same key and request | Return or resume the prior operation. |
| Same key with different input | `conflict`. |
| Crash during staging | Reuse or remove only matching owned incomplete staging. |
| Crash after version finalization but before activation | Prior `current` remains; candidate is reusable. |
| Crash after activation | Resume from receipt; compatibility metadata governs executable rollback. |
| Start fails before migration risk | Restore prior owned selector/unit/config and restart the prior build. |
| Schema is uncertain or incompatible | Keep candidate; no automatic executable rollback. |
| Local health passes and Tailscale is unavailable | Keep healthy local install; return partial exit `8`; retry Serve phase. |
| Root changes between preview and apply | Reject before mutation. |
| Root changes after local commit | Keep local install; preserve root; return partial conflict. |
| First daemon creates data and then fails | Retain all data; setup does not inspect or delete it. |
| Process dies while holding setup lock | OS releases advisory lock; receipt and staging support safe resume. |

## Remove and uninstall

### `system setup remove --yes`

Remove performs exactly these steps:

1. Acquire the setup lock and recheck every nominally owned path and handler.
2. Refuse a drifted or conflicting Serve root before deployment teardown.
3. Remove only the exact receipt-bound root handler and verify unrelated Serve state.
4. Stop the owned Observatory unit.
5. Disable the unit.
6. Remove only the matching generated user-unit file.
7. Reload the user manager.
8. Verify the unit is absent/inactive and the handler is absent.
9. Retain installed versions, `current`, `$HOME/.local/bin/obs`, configuration, and all authoritative data.
10. Retain a tombstone receipt sufficient to prevent later handler misownership.

Repeated remove is unchanged success after verification.

### `system setup uninstall --yes`

Uninstall first performs remove. It retains configuration and the complete authoritative data root. It then:

- removes `$HOME/.local/bin/obs` only when it is the expected Observatory symlink;
- removes `current` only when it is an owned selector;
- removes only verified Observatory version directories;
- removes verified install/setup receipts after preparing the final result;
- retains `${XDG_CONFIG_HOME:-$HOME/.config}/observatory/` and `config.toml`;
- retains `${XDG_DATA_HOME:-$HOME/.local/share}/observatory/` in full; and
- reports every retained path and backup guidance.

A running executable can unlink its command path safely, but it prepares all required verification and final output first. A foreign file at any nominally owned location causes `ownership_conflict`; uninstall never recursively deletes ambiguous state.

Neither removal command opens storage, removes a catalogue, deletes Artifact bytes, changes a Service, or purges a backup.

## Output and error contract

Local setup/service commands use the public one-value envelope and identify their authority:

```json
{
  "schemaVersion": 1,
  "ok": true,
  "result": {
    "authority": "local_setup",
    "command": "apply",
    "operationId": "setup_...",
    "idempotencyKey": "...",
    "changed": true,
    "partial": false,
    "ready": true,
    "phase": "verified",
    "candidate": {
      "version": "1.2.0",
      "digest": "sha256:...",
      "provenance": "verified"
    },
    "installation": {
      "currentVersion": "1.2.0",
      "previousVersion": "1.1.0",
      "stableCommand": "installed"
    },
    "systemd": {
      "unit": "observatory.service",
      "enabled": true,
      "activeState": "active",
      "subState": "running"
    },
    "daemon": {
      "endpoint": "http://127.0.0.1:3773",
      "build": "1.2.0",
      "apiVersion": 1,
      "health": "healthy",
      "startupReconciled": true
    },
    "tailscale": {
      "state": "verified",
      "canonicalOrigin": "https://desktop.greyhound-chinstrap.ts.net/",
      "rootHandler": "owned_exact",
      "remoteGrantVerification": "required"
    },
    "actions": [],
    "rollback": {
      "automaticRollbackAllowed": true,
      "previousVersionRetained": true
    },
    "warnings": []
  }
}
```

Check returns ordered checks and proposed actions. Service status reports installation, selected/prior builds, unit load/enable/active/sub states, MainPID/process identity, loopback socket owner, health or health-unavailable reason, Serve ownership, last unit result/restart count, rollback compatibility, and an exact next action.

Stable local states include:

```text
candidate: verified | missing_metadata | checksum_mismatch |
           provenance_invalid | unsupported_target
installation: absent | partial | installed | drifted
config: absent | valid | migration_required | invalid | drifted
user_manager: available | unavailable
linger: enabled | disabled | unknown
unit: absent | exact | drifted | foreign
service: active | inactive | failed | activating | deactivating | unknown
process: exact | stale | foreign | absent
health: healthy | degraded | unhealthy | offline | unavailable
serve_root: absent | owned_exact | matching_unowned | conflicting | unknown
grant: externally_verified | external_verification_required
```

Stable setup errors include:

```text
confirmation_required
contention
setup_precondition
provenance_invalid
ownership_conflict
unit_failed
health_timeout
daemon_identity_mismatch
schema_rollback_unsafe
tailscale_unavailable
serve_conflict
changed_record
internal
```

They map to settled exits: usage/confirmation `2`, conflict `4`, unavailable/timeout `5`, contention `6`, trustworthy partial `8`, and completed unhealthy state `10`.

Human success/results go to stdout; warnings, progress, and journal guidance go to stderr. JSON success emits exactly one envelope on stdout. JSON failure emits exactly one envelope on stderr with empty stdout. Results redact private paths and sensitive process or journal details.

## Security requirements

1. Setup rejects root or sudo-derived execution and operates only as the owning desktop user.
2. It installs no privileged unit and performs no package-manager, `sudo`, Polkit, or linger mutation.
3. Installed state is user-owned and private or non-group/world-writable as appropriate.
4. External release-signature and provenance verification anchors first-install trust; candidate self-checks only reinforce it.
5. Every replacement uses ownership checks, temporary files, atomic rename, file sync, and parent sync.
6. Candidate paths and version strings cannot traverse or escape fixed installation roots.
7. Subprocesses use argv arrays, fixed executable names/paths, a cleared dangerous environment, bounded timeouts, and no shell.
8. The daemon listener must be loopback; bootstrap rejects non-loopback configuration.
9. Browser mutations retain canonical Host and same-origin protections in the daemon.
10. Tailscale grants are the sole remote authorization boundary; Serve does not replace grants.
11. Tailscale identity headers provide attribution, not a second authorization system.
12. Funnel is prohibited.
13. Serve mutation is handler-specific and receipt-bound; reset, whole-config replacement, and matching-unowned adoption are prohibited.
14. Bootstrap logs and JSON redact home/storage paths, secret-bearing URLs, SQL/content, journal secrets, and teardown argv.
15. Uninstall retains configuration and all authoritative data, so local bootstrap cannot become a destructive catalogue bypass.

Tailscale recommends localhost-only origins because direct backend listeners permit spoofing of Serve identity headers; see [Tailscale Serve behavior](https://tailscale.com/docs/features/tailscale-serve).

## End-to-end acceptance journey

Implementation acceptance exercises human and JSON output where applicable and proves the adapter cannot link or call domain/storage modules.

### A. Empty machine and read-only preview

Start with no config, data root, runtime Observatory directory, unit, release tree, selector, stable command, daemon, or Serve root; linger is enabled and the downloaded release bundle was externally verified.

Prove:

1. Normal `obs system status` returns daemon unavailable without creating files or starting anything.
2. `./obs system setup check` creates nothing and previews exact first-install actions.
3. `--server` on every setup/service leaf is a usage error.
4. Invalid provenance, relative XDG paths, missing runtime, missing linger, foreign unit, occupied port, and conflicting root each fail before mutation.
5. Missing linger reports the operator command and never executes it.
6. A matching but unowned Serve root is a conflict, not adopted ownership.

### B. First apply

Prove:

1. Apply requires `--yes`; non-TTY automation also requires a caller key.
2. Concurrent apply loses bounded lock contention without effects.
3. Candidate bytes stage and sync under `.staging-*`.
4. Version finalization and selector/stable-command changes are atomic.
5. Config and unit have exact content, ownership, and permissions.
6. The user unit is enabled and started explicitly.
7. Only daemon startup creates the data root, catalogue, and storage layout.
8. Exact candidate loopback health, reconciliation, listener, and API pass; no other daemon endpoint is queried over loopback.
9. Root Serve configuration occurs last and preserves unrelated handlers.
10. The canonical HTTPS `/` returns the settled `308` redirect to `/ui/`.
11. Repeated apply returns `changed:false`.
12. Reusing a key with changed input conflicts.

### C. Canonical security

From an intended remote tailnet client, prove DNS/TLS, `/`, `/ui/`, and approved health/API behavior at the canonical origin. Prove the backend loopback port is unreachable remotely.

From an ungranted tailnet identity/device, prove TCP/HTTPS denial. From LAN-only and public clients, prove denial. Verify no Funnel listener exists and unrelated Serve handlers are byte-for-byte or canonically unchanged.

### D. Service lifecycle

Prove:

- stop exits cleanly within 30 seconds, remains stopped despite `Restart=on-failure`, and leaves Serve/data unchanged;
- start restores exact health without changing Serve, configuration, or data;
- restart serves the same authority after reconciliation;
- status never starts or installs anything;
- SIGKILL at every startup phase reaches a valid settled or diagnostic state;
- five failures in 60 seconds reach the rate limit;
- explicit start resets only Observatory's failed counter once; and
- unrelated failed user units do not block setup or service control.

### E. Upgrade and migration

Prove:

- a newer verified release stages while the old daemon remains live;
- `current` changes atomically and exact new build health follows;
- only the daemon performs pre-migration backup gating and catalogue migration;
- no-migration, compatible migration, incompatible rollback, and uncertain post-failure cases follow the declared rules;
- fault injection after every stage, sync, rename, config, unit, migration, and health boundary resumes safely;
- the prior executable remains available;
- no old executable runs against an unsupported new schema; and
- a Tailscale outage leaves a healthy loopback installation with retryable partial setup.

### F. Foreign and partial state

Prove:

- a foreign loopback listener and unmanaged daemon-lock holder are never killed;
- a stale or wrong-build health endpoint is rejected;
- a drifted or foreign unit is not overwritten without proven ownership and confirmed apply;
- a matching-unowned Serve root is not adopted;
- a root change between check/apply or immediately before mutation is rejected;
- handler removal preserves every unrelated mount; and
- no bootstrap or service command imports or invokes SQLite, catalogue, Revision storage, recovery, or domain-dispatch code.

### G. Remove, uninstall, and reinstall

Prove:

- remove deletes only the owned root handler and generated user unit while retaining installed releases, `current`, stable command, config, and data;
- repeated remove is unchanged success;
- uninstall performs remove, then deletes only verified stable-command, selector, release, and receipt state;
- config and the complete data root remain byte-identical;
- drifted root or foreign nominal path blocks destructive removal;
- a newly downloaded and externally verified release can reinstall after uninstall; and
- the restarted daemon opens or migrates retained data through normal daemon-owned rules.

## Sources

- [Issue #19: Define first-install bootstrap and setup authority](https://github.com/Whamp/observatory/issues/19)
- [Observatory public control plane](https://github.com/Whamp/observatory/blob/master/docs/research/2026-07-09-observatory-public-control-plane.md)
- [Observatory implementation stack, packaging, and supervision](https://github.com/Whamp/observatory/blob/master/docs/research/2026-07-09-observatory-implementation-stack.md)
- [Observatory storage diagnostics and recovery](https://github.com/Whamp/observatory/blob/master/docs/research/2026-07-09-observatory-storage-diagnostics-recovery.md)
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/latest/)
- [`systemctl` manual](https://www.freedesktop.org/software/systemd/man/latest/systemctl.html)
- [`systemd.service` manual](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)
- [`loginctl` manual](https://www.freedesktop.org/software/systemd/man/latest/loginctl.html)
- [Tailscale Serve CLI](https://tailscale.com/docs/reference/tailscale-cli/serve)
- [Tailscale Serve behavior](https://tailscale.com/docs/features/tailscale-serve)
- [Tailscale grants](https://tailscale.com/docs/reference/syntax/grants)

## Residual implementation proofs

- Pin and test the supported Tailscale version and fail closed on an unknown Serve JSON schema.
- Exercise positive and denied remote-grant cases from separate tailnet clients; the host cannot prove effective grants alone.
- Fix the external release signing identity and provenance tooling in release engineering before first installation.
- Fault-inject setup receipt recovery around selector activation and catalogue migration uncertainty.
