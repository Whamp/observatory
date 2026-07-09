# Observatory persistence and indexing architecture

Date: 2026-07-09

Ticket: [Choose the persistence and indexing architecture](https://github.com/Whamp/observatory/issues/11)

## Verdict

Use a hybrid local architecture:

- one SQLite catalogue is authoritative for identity, metadata, lifecycle state, Service registrations and observations, operation intents, and indexes;
- immutable Artifact Revision bytes live in checksummed directories on the same local filesystem as staging and quarantine; and
- recovery manifests are integrity and salvage evidence, never an independent source of discoverability.

SQLite fits Observatory's device-local, low-writer-concurrency workload without adding a database service. Filesystem directories keep immutable bundles efficient to serve, verify, move, and quarantine. Neither directory presence nor manifest contents independently make an Entry visible.

Publish and cleanup must use durable, restart-safe state machines because a SQLite transaction cannot atomically commit an arbitrary filesystem rename. The governing order is **durable bytes first, catalogue visibility last** for Publish and **catalogue unavailability first, quarantine move second** for cleanup. Same-filesystem atomic renames establish each byte-level visibility boundary; durable operation intents make every crash point recoverable.

## Why this model fits the contracts

The established contracts require more than a file inventory:

- Publish stages and validates a complete owned copy, exposes no partial publication, creates immutable Revisions, and atomically advances an explicitly named stable Artifact on replacement.
- Portable `.obs.json` metadata intentionally excludes Project, stable identity, publication time, slug, and Revision history.
- Services have explicit identities, atomically updated Targets, record-version-sensitive teardown, and version-scoped observations.
- Retention requires permanent tombstones and non-reuse, recovery windows, append-only audit events, quarantine, and idempotent cleanup.

These requirements come from the [Artifact contract](https://github.com/Whamp/observatory/issues/2), [Service contract](https://github.com/Whamp/observatory/issues/3), [Artifact retention policy](https://github.com/Whamp/observatory/issues/7), and [Service liveness policy](https://github.com/Whamp/observatory/issues/8).

SQLite's relational constraints and indexes can enforce one current Revision, `(Project canonical directory, Service name)` uniqueness, one primary Target, lifecycle deadlines, record versions, audit ordering, and due-work queries. Only rows in committed catalogue states are discoverable or serveable. SQLite documents device-local storage, low writer concurrency, and datasets below a terabyte as appropriate uses, and it remains embedded and serverless. See [Appropriate Uses for SQLite](https://sqlite.org/whentouse.html), [SQLite Is Serverless](https://sqlite.org/serverless.html), and [SQLite as an Application File Format](https://www.sqlite.org/appfileformat.html).

## Storage layout and authority

Use a private storage root with this conceptual layout:

```text
catalogue.sqlite
staging/<operation-id>/
revisions/<opaque-revision-id>/
quarantine/<operation-or-revision-id>/
```

Physical path components derive only from generated opaque IDs, never mutable titles, slugs, source paths, or reusable sequence-derived public identities.

Each final Revision directory contains the exact served tree plus a reserved, non-served recovery manifest. The manifest records its schema version, Artifact ID, Revision ID, entry path, logical byte and file counts, Publish instant, and each file's path, size, and digest. SQLite stores the same manifest digest. A manifest edit cannot mutate a live Entry: the catalogue remains authoritative, and a mismatch is an integrity failure.

Suggested catalogue areas are `projects`, `artifacts`, `revisions`, `revision_tombstones`, `services`, `targets`, `target_observations`, `storage_operations`, `backup_leases`, and append-only `audit_events`. Keep only the latest Target observation unless a later requirement introduces history.

Use `STRICT` tables when the chosen runtime provides SQLite 3.37 or newer, explicit `CHECK` and `UNIQUE` constraints, and `PRAGMA foreign_keys=ON` on every connection because SQLite otherwise leaves foreign-key enforcement disabled by default. Index stable public lookups, `(project_id, service_name)`, expiry and cleanup states and deadlines, superseded Revision age/order, pending operations, and probe scheduling rather than every presentation field. See [STRICT tables](https://www.sqlite.org/stricttables.html) and [SQLite foreign keys](https://sqlite.org/foreignkeys.html).

## Concurrency and local-filesystem boundary

Use WAL only on a local filesystem, `synchronous=FULL` for durable catalogue commits, a bounded busy timeout, and short `BEGIN IMMEDIATE` write transactions. All mutating API and CLI requests flow through the backend. Uniqueness constraints and compare-and-swap record versions are the final guards against duplicate creation, late probe results, stale teardown, and cleanup races.

WAL allows concurrent readers while retaining a single writer, but it depends on shared memory and is not suitable for a network filesystem. Observatory storage must therefore reject or explicitly flag unsupported remote filesystems rather than promise durability there. A bounded retry may handle brief `SQLITE_BUSY` contention; exhausted retries become an explicit contention error. See SQLite's documentation for [WAL](https://sqlite.org/wal.html), [transactions](https://www.sqlite.org/lang_transaction.html), and [busy timeouts](https://sqlite.org/c3ref/busy_timeout.html).

## Atomic Publish protocol

Publish is a two-resource state machine:

1. Preflight capacity and validate the source boundary. Allocate never-reused Artifact, Revision, and operation IDs. Commit a `staging` operation record without changing the live Artifact identity.
2. Copy into `staging/<operation-id>` on the same filesystem as `revisions/`. Reject unsafe file types and links, compute the manifest, then sync every created file and required directory.
3. Atomically rename the complete staging directory to `revisions/<revision-id>` and sync the parent directory. Prefer descriptor-relative path operations with no-follow semantics to avoid path races.
4. In one short SQLite transaction, verify operation and record-version expectations, insert the immutable Revision and audit event, update the stable Artifact's current Revision and retention deadline, and mark the operation committed. Visibility begins only at this commit.

A failure before the catalogue transaction leaves the prior current Revision untouched. A crash after the durable rename but before the transaction leaves undiscoverable bytes that recovery can reconcile through the operation record and matching manifest.

Linux `rename()` provides an atomic directory-entry change on the same filesystem. File sync alone does not persist the containing directory entry, which makes directory sync part of the durability protocol. See [rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html), [fsync(2)](https://www.man7.org/linux/man-pages/man2/fsync.2.html), and the [openat(2) rationale](https://man7.org/linux/man-pages/man2/openat.2.html).

## Recovery and cleanup protocols

Before accepting byte-adding writes at startup, inspect nonterminal `storage_operations` and only Observatory-owned staging and quarantine paths:

- a valid durable final Revision with matching operation and manifest may complete the catalogue transaction;
- an intact staging directory may resume finalization;
- malformed or mismatched data moves to quarantine and records an error; and
- an unreferenced final directory moves to quarantine rather than being automatically adopted.

Never infer a successful Publish or purge merely because a path is present or absent. Read-only discovery and serving can continue for committed catalogue rows whose bytes are intact.

Cleanup reverses Publish's ordering. In a catalogue transaction, re-check eligibility, record version, pin, and current-Revision constraints; append intent and audit records; and mark the complete Revision `quarantining`, making it explicitly unavailable. Then atomically rename its directory into same-filesystem quarantine and sync both parent directories. A final transaction records the tombstone, reclaimed bytes, and terminal state. Physical deletion is idempotent and may follow asynchronously. Missing or mismatched bytes are a reconciliation error, not proof that deletion succeeded.

## Backup, rebuild, repair, and migration

A consistent backup binds a catalogue snapshot to its immutable bytes. In a short transaction, create a backup lease naming the exact committed Revisions in scope and preventing their cleanup. Use SQLite's Online Backup API for a consistent database snapshot, copy or link the leased Revision directories into backup staging, verify every manifest and digest, write a top-level manifest binding the snapshot to the Revision set, sync it, atomically finalize the backup, and release the lease. Do not copy only `catalogue.sqlite` while WAL is active, and do not claim a database-only backup includes Artifact bytes. See the [SQLite Online Backup API](https://sqlite.org/backup.html) and [WAL file-set documentation](https://sqlite.org/wal.html).

Rebuild and repair have different guarantees. Valid Revision manifests can reconstruct Artifact byte inventory, but they cannot fully recover stable current selection, retention and pin state, audit history, Services, teardown actions, or observations. Rebuild therefore creates a candidate catalogue, reports recovered, lost, and ambiguous records, and requires operator validation before atomic replacement. It never silently promotes orphan bytes.

Normal repair runs `PRAGMA quick_check`, `PRAGMA integrity_check`, and `PRAGMA foreign_key_check`, then reconciles manifest digests and catalogue-to-filesystem references. SQLite's `.recover` command or recovery API can salvage a corrupt database into a separate database, but recovered constraints may be invalid. Treat that output as evidence for controlled reconstruction, not as an automatically trusted replacement. See [SQLite PRAGMAs](https://sqlite.org/pragma.html) and [Recovering Data From A Corrupt SQLite Database](https://www.sqlite.org/recovery.html).

Schema migrations are transactional, monotonic, and fail closed. Set an Observatory-specific `application_id`; track the schema with application-controlled `user_version`; run ordered migrations under an exclusive application startup/migration lock and a SQLite transaction; update the version in that transaction; and run foreign-key and quick integrity checks before enabling writes. Refuse to write a newer unknown schema, retain a pre-migration Online Backup snapshot, and use SQLite's documented table-rebuild procedure where `ALTER TABLE` is insufficient. See [application_id and user_version](https://sqlite.org/pragma.html) and the [ALTER TABLE migration procedure](https://www.sqlite.org/lang_altertable.html).

## Alternatives rejected

- **Filesystem as source of truth:** cannot faithfully model Services, observations, atomic multi-record invariants, audit history, tombstones, retention deadlines, or concurrent updates without recreating a database.
- **Authoritative per-Entry manifests or sidecars:** improve salvageability but create cross-file transaction, locking, migration, and split-brain problems.
- **Complete bundles as SQLite BLOBs:** simplify one-file database backup but make static serving, directory-level quarantine, and independent immutable-byte verification less direct while enlarging catalogue backup and repair work.
- **PostgreSQL, object storage, or an external search engine:** add supervision and operational weight without a distributed or multi-host requirement. Multi-host replication is explicitly outside the [Wayfinder map](https://github.com/Whamp/observatory/issues/1).

## Diagnostics boundary

Storage diagnostics must separately expose SQLite open and extended error codes; quick, integrity, and foreign-key checks; WAL and checkpoint state; pending storage operations; missing or mismatched Revision paths; manifest parse, version, and digest failures; staging and quarantine age and bytes; cleanup and backup-lease failures; and free-space reserve.

Database corruption, Artifact byte corruption, an interrupted Publish, and an unreachable external Service are distinct states and must never collapse into one generic “offline” result. Exact operator commands, machine-readable failure schema, confirmation UX, and restore cutover belong to the follow-up ticket “Specify storage diagnostics and recovery operations,” not this architecture decision.

## Implementation constraints left open

The implementation stack is intentionally unresolved here. Exact APIs for recursive no-follow copying and directory syncing follow [Choose the implementation stack, packaging, and supervision model](https://github.com/Whamp/observatory/issues/12).

The syscall evidence above is Linux-specific. Filesystem capability detection and durability behavior require runtime-specific validation before supporting macOS, Windows, or any remote filesystem.
