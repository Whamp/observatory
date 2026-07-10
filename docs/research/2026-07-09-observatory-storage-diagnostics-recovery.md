# Observatory storage diagnostics and recovery

Date: 2026-07-09

Ticket: [Specify storage diagnostics and recovery operations](https://github.com/Whamp/observatory/issues/16)

## Summary

Observatory exposes three diagnostic profiles and one plan/apply recovery surface under the approved `obs system` namespace, plus backup create/verify and the settled `obs cleanup` controls. SQLite remains the sole authority: manifests, filesystem contents, and SQLite `.recover` output are evidence used to build and validate separate candidates, never grounds for automatic adoption or in-place destructive repair.

The stable machine model separates command execution (`ok`) from storage health (`healthy`, `degraded`, `unhealthy`, `offline`) and returns an ordered result for every requested check. Recovery is intent-driven, idempotent, previewable, maintenance-locked when authority or bytes can change, crash-resumable, and auditable. Catalogue replacement, availability-changing quarantine, restore, and permanent discard require an exact preview and `--yes`.

## Diagnostic profiles and availability gates

SQLite distinguishes the relatively fast `quick_check` from the more complete `integrity_check`: `quick_check` omits UNIQUE and index/table consistency work. Neither check detects foreign-key violations, which require `foreign_key_check`. Observatory therefore uses three profiles rather than one ambiguous health check. See the [SQLite PRAGMA documentation](https://sqlite.org/pragma.html#pragma_integrity_check).

| Profile and command | Checks | Mutation and availability contract |
| --- | --- | --- |
| **Fast status** — `obs system status` | Storage root/filesystem support; capacity and reserve; read-only SQLite open plus extended error; `application_id`; schema version; WAL presence/size and last-known checkpoint failure; counts/ages of nonterminal intents, staging, quarantine, leases, and cleanup failures; existence of every live current Revision path, without full tree or digest checks. | Never writes and never waits behind long work. Normal writes, probes, and serving continue unless a fail-closed state below is found. Suitable for startup, readiness, and frequent polling. |
| **Normal diagnostics** — `obs system diagnostics` | All fast checks; `quick_check`; `foreign_key_check`; passive checkpoint observation; every catalogue-to-Revision path; manifest parse/version/manifest digest; intent state-machine consistency; staging/quarantine inventory; lease ownership/expiry; cleanup error details. It does not hash every content file. | Read-only and online. A bounded snapshot is used; concurrent mutations may continue. Checks that cannot obtain a consistent snapshot return `skipped/contention`, not a false pass. |
| **Deep/offline** — `obs system diagnostics --deep` | All normal checks; full `integrity_check`; every manifest-to-content path, size, and digest; complete orphan scan inside Observatory-owned directories; backup verification when explicitly named; active filesystem durability/capability test in a disposable owned probe directory, including file/directory sync and same-mount rename. | Acquires the global maintenance gate. New mutations, cleanup, probe dispatch/result recording, index/API catalogue reads, and Artifact serving fail closed with an explicit maintenance response until completion. External Services are not stopped, restarted, killed, or probed. The probe directory is the only write and is removed; probe or removal failure is reported. |

Fail-closed behavior depends on the failed category:

- `catalogue_unavailable`, `wrong_application`, `schema_invalid`, `schema_newer`, `catalogue_corrupt`, or an unsupported storage filesystem rejects all catalogue writes and probes. Observatory does not serve or discover Entries from untrusted authority.
- `revision_missing`, `content_corrupt`, or `manifest_invalid` makes only affected Revisions unavailable immediately. Observatory rejects byte-adding Publish/import/replace until reconciliation; healthy catalogue metadata operations and Service probes may continue.
- An interrupted Publish or cleanup intent must be completed or classified during startup reconciliation before byte-adding writes. Intact committed Revisions remain serveable.
- A breached free-space reserve or persistent cleanup failure rejects byte-adding writes and allows cleanup, recovery, and intact reads. Metadata-only writes and Service probes continue while SQLite is healthy.
- Checkpoint contention alone is degraded and retryable; it does not stop service. WAL I/O or corruption fails catalogue writes closed.
- A stale backup lease blocks cleanup only for its named Revisions until reconciliation. It neither authorizes lease deletion nor blocks unrelated operations.
- Service reachability (`online`, `offline`, `unknown`, `stale`) remains a separate Service observation and never changes storage health. The storage diagnostic envelope may link to a separate Service summary but does not include `service_unreachable` as a storage check.

## Check taxonomy

Every ordered check result has:

- `id`;
- `status`: `pass`, `warn`, `fail`, `error`, or `skipped`;
- a check-specific stable `state`;
- a stable `category`;
- `message`;
- `retryable`;
- `scope`;
- timestamps and duration; and
- redacted `details`.

SQLite open errors retain primary and extended result codes because extended codes add actionable detail to the primary category. WAL checkpoint observations retain SQLite's three values—busy result, total WAL frames, and checkpointed frames—because a passive checkpoint can make progress without waiting and can report incomplete work instead of a generic failure. See the [SQLite open API](https://sqlite.org/c3ref/open.html), [result codes](https://sqlite.org/rescode.html), and [checkpoint API](https://sqlite.org/c3ref/wal_checkpoint_v2.html).

Required check IDs and states are:

| Check ID | Stable states, not free-form messages |
| --- | --- |
| `sqlite.open` | `open`, `not_found`, `permission_denied`, `busy`, `io_error`, `not_database` |
| `sqlite.application` | `matches`, `mismatch`, `unreadable` |
| `sqlite.schema` | `supported`, `older_migration_required`, `newer_unsupported`, `invalid` |
| `sqlite.quick` | `ok`, `violations`, `not_run` |
| `sqlite.integrity` | `ok`, `violations`, `not_run` |
| `sqlite.foreign_keys` | `ok`, `violations`, `not_run` |
| `sqlite.wal` | `clean`, `frames_pending`, `checkpoint_blocked`, `wal_io_error`, `not_wal` |
| `storage.intents` | `terminal`, `interrupted_resumable`, `interrupted_ambiguous`, `invalid_transition` |
| `revision.path` | `present`, `missing`, `wrong_type`, `unexpected` |
| `revision.manifest` | `valid`, `missing`, `parse_error`, `unsupported_version`, `catalogue_digest_mismatch` |
| `revision.content` | `valid`, `missing_bytes`, `size_mismatch`, `digest_mismatch`, `unsafe_member`, `not_run` |
| `storage.staging` | `clear`, `active`, `abandoned`, `unowned_or_ambiguous` |
| `storage.quarantine` | `clear`, `retained`, `orphaned`, `purge_failed` |
| `storage.backup_leases` | `active`, `expired_releasable`, `stale_ambiguous`, `invalid_scope` |
| `storage.cleanup` | `ok`, `interrupted`, `candidate_failed`, `persistent_failure` |
| `storage.filesystem` | `supported`, `read_only`, `cross_mount_layout`, `remote_or_unsupported`, `capability_failed` |
| `storage.capacity` | `within_reserve`, `reserve_at_risk`, `reserve_breached`, `capacity_unknown` |

Stable categories form a deliberately small action taxonomy: `catalogue`, `schema`, `integrity`, `wal`, `content`, `operation_interrupted`, `missing_bytes`, `quarantine`, `lease`, `cleanup`, `filesystem`, `capacity`, `contention`, `permission`, and `internal`. Messages add specificity without becoming API.

The distinctions required by issue #16 map to the taxonomy as follows:

- Database corruption: `sqlite.quick|integrity = violations`, category `integrity`.
- Artifact byte corruption: `revision.content = size_mismatch|digest_mismatch`, category `content`.
- Interrupted Publish or cleanup: `storage.intents = interrupted_*`, category `operation_interrupted`, with operation kind and last durable phase.
- Missing bytes: `revision.path = missing` or `revision.content = missing_bytes`, category `missing_bytes`.
- Quarantined or orphaned bytes: `storage.quarantine = retained|orphaned` or path `unexpected`, category `quarantine`. This is not corruption unless validation proves it.
- Stale lease: `storage.backup_leases = expired_releasable|stale_ambiguous`, category `lease`.
- Free reserve: `storage.capacity = reserve_at_risk|reserve_breached`, category `capacity`.
- Unreachable Services: only Service reachability output, never a storage category.

## Machine result model

Diagnostics follow [issue #9's one-envelope, ordered-partial JSON contract](https://github.com/Whamp/observatory/issues/9#issuecomment-4931095968). A successfully executed diagnostic returns `ok:true` even when health is bad: `ok` means the command returned trustworthy requested results, while `result.health` says whether storage is safe. A command-level failure—daemon unavailable, invalid usage, missing confirmation, or inability to establish any trustworthy diagnostic context—uses `ok:false` on stderr.

Mixed or unavailable checks set `partial:true`; no omitted requested check is silently treated as a pass.

```json
{
  "schemaVersion": 1,
  "ok": true,
  "result": {
    "profile": "normal",
    "health": "unhealthy",
    "partial": true,
    "generatedAt": "2026-07-09T23:00:00Z",
    "checks": [
      {
        "id": "sqlite.quick",
        "status": "fail",
        "state": "violations",
        "category": "integrity",
        "message": "catalogue integrity violations found",
        "retryable": false,
        "scope": {"kind": "catalogue"},
        "details": {"violationCount": 2, "reportedCount": 2}
      },
      {
        "id": "revision.content",
        "status": "skipped",
        "state": "not_run",
        "category": "contention",
        "message": "deep content verification was not requested",
        "retryable": true,
        "scope": {"kind": "all_revisions"},
        "details": {}
      }
    ],
    "recommendedOperations": ["salvage_catalogue_candidate"]
  }
}
```

Preserve caller-requested and deterministic catalogue order. Fan-out results use `items` in input or identity order with per-item `status`; set `partial:true` when outcomes mix or any requested work is `error` or `skipped`. Human output may summarize, but JSON never truncates without `details.truncated:true`, counts, and a continuation or reference.

Exit codes have these meanings:

- `0`: healthy or degraded, with every requested check completed;
- `8`: trustworthy partial results; and
- `10`: completed diagnostics found unhealthy storage.

Safe redaction excludes absolute source, storage, and home paths; teardown argv; secret-bearing URLs; raw SQL or page data; file contents; and `.recover` rows. Output may expose opaque IDs, safe relative Revision member paths, basenames, byte counts, digests rather than content, SQLite numeric and symbolic codes, phases, and audit or plan IDs.

## Plan/apply recovery contract

Use a small plan/apply interface with explicit operation kinds instead of overlapping repair commands:

```text
obs system status
obs system diagnostics [--deep]
obs system recovery preview OPERATION [SELECTOR...]
obs system recovery apply PLAN --yes [--idempotency-key KEY]
obs system recovery resume [OPERATION] [--idempotency-key KEY]
obs system backup create DESTINATION [--idempotency-key KEY]
obs system backup verify BACKUP [--deep]
obs cleanup preview [--pressure]
obs cleanup run --yes [--pressure] [--idempotency-key KEY]
```

`OPERATION` is a stable capability name, not an arbitrary script: `reconcile`, `quarantine`, `repair_catalogue_candidate`, `salvage_catalogue_candidate`, `rebuild_catalogue_candidate`, `validate_candidate`, `activate_candidate`, `restore_backup`, or `discard`.

Preview is read-only. It returns a durable plan with a plan ID, exact input identities and digests, required health generation, operation kind, effect and availability, estimated bytes, ambiguity and loss report, preconditions, confirmation requirement, rollback point, and expiry. Apply accepts only that plan, rejects changed fingerprints or health generation, and never broadens scope. This keeps two recovery verbs while making every capability discoverable in help and machine output.

Every operation follows these guarantees:

- All mutations go through the daemon; the CLI never opens SQLite or storage directly.
- Every apply or resume has a durable operation intent before its first external effect, a stable operation ID, canonical request fingerprint, phase, and terminal result. The same idempotency key and fingerprint returns or resumes the same operation. A different fingerprint conflicts. Terminal effects are never repeated.
- One global maintenance lock serializes candidate activation, restore, and any operation that changes catalogue authority. Per-Revision locks serialize reconcile, quarantine, cleanup, and backup leases. Bounded lock contention is explicit and retryable and changes nothing.
- Preconditions are checked at preview and immediately before every irreversible phase. A stale plan fails `changed_record`; Observatory never silently regenerates it.
- A crash leaves either the old authority or the new candidate authority, never a half-selected catalogue. Restart reads the durable intent and resumes or rolls back the incomplete phase. Missing paths are evidence, never proof that an effect succeeded.
- `reconcile` is the only automatic startup recovery capability. It may complete a recorded Publish whose final Revision, operation identity, manifest, and catalogue expectations all match; resume intact staging; finish a recorded cleanup quarantine/tombstone phase; release an expired lease only when its owning backup is terminal; and quarantine malformed, mismatched, or unreferenced owned bytes. It never adopts an unreferenced Revision or invents an Artifact.
- `resume` resumes an existing nonterminal intent only. It cannot reinterpret evidence as a different operation; ambiguous state stops for a new preview.
- `quarantine` atomically moves selected owned bytes to same-filesystem quarantine after recording intent and makes a committed Revision unavailable before the move. Quarantine preserves bytes and identity evidence. It is idempotent and reversible only through a separately validated reconciliation or restore plan. Quarantining an already unavailable orphan is conservative; quarantining a committed Revision requires preview and `--yes`.

Linux `rename()` is atomic at the directory-entry boundary only on the same mounted filesystem and returns `EXDEV` across mounts. `fsync()` on a file does not sync the containing directory entry, so a successful recovery phase includes required file and parent-directory durability before advancing its intent. See [rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html) and [fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html).

## Repair, salvage, rebuild, and activation

Repair, salvage, and rebuild all create candidates. None overwrites the only copy. SQLite warns that recovery output is "always suspect," may resurrect deleted content from freelist pages, and may place unassociated rows in `lost_and_found`; it cannot become authority automatically. See the [SQLite recovery documentation](https://sqlite.org/recovery.html).

### Normal repair candidate

`repair_catalogue_candidate` requires a readable catalogue with known application and schema identity. It takes a consistent snapshot, reconstructs only application-derivable structures and state under the known schema, runs migrations only through settled migration rules, and preserves the source untouched. It does not claim to cure unknown corrupt rows.

### Corrupt-catalogue salvage candidate

`salvage_catalogue_candidate` runs SQLite recovery into separate evidence, then imports into a fresh candidate only records that satisfy current Observatory schema, identity, constraint, and cross-resource validation. It reports every accepted, rejected, lost, ambiguous, and synthesized record. `.recover` SQL and rows never directly replace or mutate the source.

### Catalogue rebuild candidate

`rebuild_catalogue_candidate` starts from an empty current schema and uses valid Revision manifests and content only as evidence for byte inventory. It reports the architecture's unrecoverable or ambiguous fields: current Revision choice, retention and pin state, tombstones, audit history, Services, Targets, teardown actions, and observations. It creates no visible Entries merely because directories exist. Any proposed recovered Artifact remains candidate-only until operator validation.

### Candidate validation and activation

Candidate validation runs application and schema checks; quick, integrity, and foreign-key checks; intent checks; full Revision manifest and content checks; uniqueness checks; tombstone checks; and referential checks. It emits an explicit loss and ambiguity report. Validation cannot waive failed checks; operator resolutions produce a new candidate and plan.

Candidate activation requires a fully passing candidate, an offline maintenance lock, a fresh exact preview, and `--yes`. It durably stages the candidate, atomically selects it as catalogue authority with WAL sidecars handled as a unit, retains the former catalogue and all non-selected evidence as rollback material, and records activation in the candidate audit log. It never edits the former or only copy in place.

## Backup, restore, and discard

### Backup creation and verification

Backups bind a consistent catalogue snapshot to leased immutable bytes. SQLite's Online Backup API produces a consistent snapshot of a live database, including changes present through WAL; copying only the main database file while WAL is active can omit committed state. See the [SQLite Backup API](https://sqlite.org/backup.html) and [SQLite WAL documentation](https://sqlite.org/wal.html).

`backup create` first commits a lease naming the exact committed Revisions. It then takes an Online Backup snapshot, copies those immutable Revision directories, writes a top-level manifest binding catalogue digest, schema, and application ID to the exact Revision, manifest, and content digests, syncs, atomically finalizes, verifies, and releases the lease. Failure preserves a resumable intent or clearly incomplete backup; incomplete output is never reported as valid.

`backup verify` is read-only. It checks the top-level manifest, catalogue checks, exact Revision set, and all manifests and digests for `--deep`, then reports missing, extra, or mismatched bytes. A database-only file is reported as `catalogue_only`, never as a complete Observatory backup.

### Restore

`restore_backup` never writes over the live catalogue or Revision tree. It verifies into isolated staging, builds a restore candidate, and reports:

- Entries, bytes, and audit events newer than the backup that would be lost;
- conflicts with existing opaque IDs;
- required capacity; and
- exact cutover and rollback paths.

Apply requires an offline lock and `--yes`. Cutover durably installs the complete candidate bytes first and catalogue authority last. A crash preserves old authority until final selection or resumes the recorded cutover. The old catalogue and displaced bytes remain quarantined rollback material until separately discarded.

### Permanent discard

`discard` is the only irreversible storage operation. It can target a named quarantined object, obsolete candidate, retained pre-cutover catalogue, incomplete backup, or Artifact eligible for the settled purge contract.

Preview lists exact opaque identities, digests, bytes, recovery consequences, and why no live catalogue or lease references each target. Apply requires `--yes`, a matching unexpired plan, no active lease or reference, and a durable intent. Deletion is idempotent; partial unlink or sync failures remain retryable and are reported per item.

Discard cannot remove the only catalogue copy, the active candidate, a live current Revision, or evidence required by a nonterminal operation. Normal retention remains `obs cleanup preview` and `obs cleanup run --yes`. Recovery discard does not bypass the seven-day recovery window unless the preview names the exact Artifact and explicitly identifies early permanent purge under the [issue #7 contract](https://github.com/Whamp/observatory/issues/7#issuecomment-4930161768).

## Audit and restart guarantees

Append-only catalogue audit events record:

- diagnostics that change health gates;
- plan creation and expiry;
- operation start, resume, block, failure, and completion;
- automated reconciliation decisions;
- quarantine and restore;
- backup lease acquisition, release, and stale handling;
- candidate creation, validation, activation, and rollback;
- cleanup and discard;
- confirmation actor;
- affected opaque IDs;
- before and after digests;
- bytes;
- error category; and
- durability phase.

When the active catalogue is unreadable, the durable recovery intent or receipt alongside the candidate is non-authoritative evidence. Activation imports its chain and digest into the new candidate's audit log before selection. The receipt never makes Entries discoverable.

Rollback is an explicit new activation plan. Only retained, fully validated old authority and bytes may be reactivated under the same offline, preview, confirmation, and audit rules. A failed pre-cutover operation deletes nothing authoritative. A post-cutover problem leaves the newly active candidate authoritative until explicit rollback. Observatory never automatically chooses between competing catalogues after restart.

## Examples

These examples define outcomes, not a language, library, process manager, or filesystem-probe implementation.

```text
obs system status --json
# health=degraded: storage.capacity/reserve_at_risk;
# Artifact serving and probes continue, byte-adding writes are blocked only at reserve_breached.

obs system diagnostics --deep --json
# Acquires maintenance gate; reports sqlite.integrity separately from
# revision.content and never runs Service reachability probes.

obs system recovery preview reconcile --json
obs system recovery apply rplan_01J... --yes \
  --idempotency-key reconcile-2026-07-09 --json
# Completes only matching durable intents; unexpected Revision directories go
# to quarantine, never into the catalogue.

obs system recovery preview salvage_catalogue_candidate --json
obs system recovery apply rplan_01K... --yes \
  --idempotency-key salvage-2026-07-09 --json
# Produces a separate candidate and loss/ambiguity report; does not activate it.

obs system recovery preview validate_candidate candidate_01M... --json
obs system recovery apply rplan_01N... --yes --json
obs system recovery preview activate_candidate candidate_01M... --json
obs system recovery apply rplan_01P... --yes --json

obs system backup create /mnt/backup/obs-2026-07-09 \
  --idempotency-key backup-2026-07-09 --json
obs system backup verify /mnt/backup/obs-2026-07-09 --deep --json
obs system recovery preview restore_backup /mnt/backup/obs-2026-07-09 --json

obs cleanup preview --pressure --json
obs cleanup run --pressure --yes \
  --idempotency-key pressure-cleanup-2026-07-09 --json
# Per-candidate failures are ordered partial results; no live current Revision
# or leased Revision is evicted.
```

## Sources

- [Issue #16](https://github.com/Whamp/observatory/issues/16) defines the planning scope and fixed architecture boundary.
- [Issue #9 resolution](https://github.com/Whamp/observatory/issues/9#issuecomment-4931095968) fixes resource namespaces, one-envelope JSON, ordered partial results, idempotency, confirmation, daemon authority, and exit categories.
- [Issue #7 resolution](https://github.com/Whamp/observatory/issues/7#issuecomment-4930161768) fixes cleanup ordering, free reserve, quarantine, recovery window, and audit behavior.
- [Issue #15 resolution](https://github.com/Whamp/observatory/issues/15#issuecomment-4930243697) fixes explicit import, no discovery or adoption, source privacy, and normal Publish state-machine semantics.
- [Observatory persistence and indexing architecture](https://github.com/Whamp/observatory/blob/master/docs/research/2026-07-09-observatory-persistence-architecture.md) fixes SQLite authority, immutable same-filesystem Revision layout, intent ordering, manifests as evidence, backup, and candidate recovery.
- [SQLite PRAGMA documentation](https://sqlite.org/pragma.html) defines application and schema identifiers and quick, integrity, foreign-key, and checkpoint checks.
- [SQLite WAL](https://sqlite.org/wal.html) and the [checkpoint API](https://sqlite.org/c3ref/wal_checkpoint_v2.html) define the WAL file set, concurrency, checkpoint progress, and busy semantics.
- [SQLite recovery](https://sqlite.org/recovery.html) establishes recovered output as suspect and documents lost and freelist data behavior.
- [SQLite Backup API](https://sqlite.org/backup.html) defines consistent live catalogue snapshot semantics.
- [SQLite open API](https://sqlite.org/c3ref/open.html) and [result codes](https://sqlite.org/rescode.html) define open modes and stable primary and extended diagnostics.
- [rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html), [fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html), and [statvfs(3)](https://man7.org/linux/man-pages/man3/statvfs.3.html) define same-mount atomic rename, durability, and capacity evidence on Linux.

## Implementation-time gaps

The contract does not choose an implementation stack. Implementation must validate runtime-specific filesystem classification, including which local filesystems pass the durability probe; platform support beyond Linux; exact lock primitives; and the mechanism for atomic catalogue-selector cutover. Those decisions must preserve the observable `supported` and `remote_or_unsupported` states and the crash-safety guarantees above.
