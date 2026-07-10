# Observatory backup destination and transport topology

Date: 2026-07-09

Ticket: [Define backup destination and transport topology](https://github.com/Whamp/observatory/issues/18)

## Verdict

A backup create request names one **exact, absolute host-local final directory**. Observatory creates a consistent SQLite snapshot and an exact Revision lease in its private source-side `backups/<backup-id>/` workspace, copies the bundle into a hidden sibling directory on the destination filesystem, verifies and syncs the copy, writes a completion marker, and publishes it with same-destination-filesystem `renameat2(..., RENAME_NOREPLACE)`.

The cross-filesystem step is a verified copy, never a rename. The only visibility transition is the no-replace rename from destination-local staging to the exact `DESTINATION`. This avoids Linux's `EXDEV` boundary while retaining atomic publication and strict no-overwrite semantics. See [rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html).

SQLite `storage_operations`, backup-lease rows, and the catalogue's backup records are the sole authority for operation and lease state. Files under `backups/<backup-id>/` and destination staging are recovery evidence checked against SQLite. They never become a competing source of authority.

A bundle is valid only at its final name, with a completion marker that binds the top-level manifest and a manifest that binds the SQLite snapshot to the exact leased Revision inventory. Incomplete output is never valid. Restore never adopts external bytes in place: it copies a deeply verified bundle into Observatory-owned candidate staging and uses the settled offline candidate-activation protocol.

## Fixed boundaries

This decision preserves these settled contracts:

1. SQLite is the sole catalogue authority. Manifests, receipts, identity files, and filesystem presence are evidence only. [Issue #11 resolution](https://github.com/Whamp/observatory/issues/11#issuecomment-4930220621)
2. Revisions are immutable and cleanup cannot remove a Revision named by an active exact backup lease. [Persistence architecture](https://github.com/Whamp/observatory/blob/master/docs/research/2026-07-09-observatory-persistence-architecture.md)
3. Incomplete output is never a backup.
4. Restore creates and validates a separate candidate and never overwrites live authority. [Issue #16 resolution](https://github.com/Whamp/observatory/issues/16#issuecomment-4931158568)
5. Operations are daemon-owned, intent-driven, idempotent, crash-resumable, and auditable. Public envelopes, ETags, preconditions, and idempotency follow the [issue #17 control-plane contract](https://github.com/Whamp/observatory/issues/17#issuecomment-4931421145).
6. Source-side backup workspace bytes count toward Observatory capacity and preserve the greater-of-1-GiB-or-5% reserve. [Issue #7 resolution](https://github.com/Whamp/observatory/issues/7#issuecomment-4930161768)

## Topology and authority

```text
Observatory private filesystem                   Explicit destination filesystem
──────────────────────────────                   ───────────────────────────────
catalogue.sqlite                                 <parent>/
backups/<backup-id>/                               .<leaf>.observatory-<id>.incomplete/
  catalogue.sqlite          source snapshot          catalogue.sqlite
  operation.json            recovery evidence        revisions/<revision-id>/...
  receipt.json              recovery evidence        backup-manifest.json
  ...                                              .observatory-complete
                                                  DESTINATION/  ← atomic rename
```

`backups/<backup-id>/` is the source-side snapshot, workspace, and receipt directory. It is not a retained exported Revision payload:

- `catalogue.sqlite` is the source snapshot used for copy and resume.
- `operation.json`, any workspace identity file, and `receipt.json` are non-authoritative recovery evidence. Reconciliation accepts them only when they match authoritative SQLite operation, lease, and backup rows.
- Revision payload bytes are copied directly from leased immutable `revisions/<revision-id>/` directories. They are not duplicated under the source backup workspace.
- Completed exported payload lives only at `DESTINATION`.
- The source workspace can be removed after durable completion and lease release. Authoritative terminal operation and audit rows remain.
- A directory's presence under `backups/` cannot create, complete, cancel, renew, or release an operation or lease.

The daemon commits authoritative intent before any external effect. It never infers completion from a path alone or adopts an unowned directory.

## Exact destination and backup identity

### `DESTINATION`

```text
obs system backup create DESTINATION
```

`DESTINATION` is the exact final directory to create, not a parent and not a naming template.

- It must be an absolute host-local path.
- Its parent must already exist as an accessible directory.
- The final leaf must not exist as any inode type.
- Observatory creates no missing ancestors and chooses no alternate or suffixed final name.
- Relative paths, `/`, empty or ambiguous leaves, `.` or `..` components, and symlinked components are rejected.
- Request fingerprinting uses the exact normalized API value defined by the control plane; CLI normalization cannot silently change the server fingerprint.

For example:

```text
obs system backup create /mnt/offline/observatory-2026-07-10
```

creates exactly `/mnt/offline/observatory-2026-07-10/`.

### Backup IDs

Each create allocates a never-reused random 128-bit ID in Observatory's 26-character lowercase Crockford-base32 form. The ID:

- appears in the authoritative operation and backup rows, source evidence, top-level manifest, completion marker, and audit event;
- selects backup status, resume, cancellation, verify-by-ID, and discard;
- does not derive from a path, time, host, contents, or idempotency key; and
- remains valid when an operator moves a complete bundle because verification binds embedded identity and digests, not an external path.

Human output labels it `Backup`; machine output uses `backupId`.

### Hidden destination staging

For final leaf `observatory-2026-07-10`, staging is:

```text
.observatory-2026-07-10.observatory-<backup-id>.incomplete
```

Observatory creates it as a sibling with mode `0700`, no-follow, and no-replace semantics. Any collision is an error unless authoritative SQLite identifies the same nonterminal operation and every non-authoritative identity field matches that operation, request fingerprint, backup ID, source snapshot digest, lease set, destination-parent fingerprint, and expected staging name. Observatory never adopts or deletes an unknown colliding object.

A staging directory remains invalid even after all payload and marker bytes have been written. Only the exact final name can be valid.

## Strict no-overwrite rules

1. Existing `DESTINATION` returns `destination_exists` for every inode type.
2. A nonmatching staging object returns `staging_collision`.
3. An identical idempotency replay returns, resumes, or reports the original operation.
4. Reusing a key with a different canonical fingerprint returns `idempotency_conflict`.
5. A new request never resumes, merges with, overwrites, or deletes another operation's staging.
6. Finalization uses `renameat2(..., RENAME_NOREPLACE)`. A final-leaf race returns `destination_exists` and leaves the competing inode untouched.
7. There is no `--force`, overwrite, merge, incremental update, deduplication, hard-link farm, or `latest` alias.

## Destination filesystem gate

### Allowlist and rejection

The initial write-capable destination allowlist is Linux btrfs, ext4, and XFS. An allowlisted filesystem must also pass the active probe below. The allowlist is mandatory: passing the probe cannot admit another filesystem class.

Reject:

- NFS and every other network filesystem;
- CIFS/SMB;
- FUSE, including sshfs and rclone mounts;
- 9p and virtiofs;
- overlayfs;
- tmpfs, ramfs, and other memory-backed filesystems;
- procfs, sysfs, device, configuration, and control filesystems;
- object-storage gateways;
- read-only mounts;
- unknown filesystem types; and
- mounts where stable reads, capacity reporting, file or directory sync, or no-replace rename cannot be proved.

Use `statfs` and `/proc/self/mountinfo` to classify the effective mount and reject remote or unsupported types. Use `statx` mount identity, preferring `STATX_MNT_ID_UNIQUE` where available, rather than `st_dev` alone. Staging and the final parent must retain the same mount ID, filesystem type, mount options, and parent inode from preflight through finalization. A change returns `destination_changed`. See [statfs(2)](https://man7.org/linux/man-pages/man2/statfs.2.html), [proc_pid_mountinfo(5)](https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html), and [statx(2)](https://man7.org/linux/man-pages/man2/statx.2.html).

### Active capability probe

Inside a new randomly named directory in the destination parent, Observatory must:

1. create two regular files with `O_CREAT|O_EXCL|O_NOFOLLOW`;
2. write nontrivial known bytes to one file;
3. sync the file and verify its length and content;
4. sync the probe directory;
5. rename the file with `renameat2(RENAME_NOREPLACE)`;
6. prove a no-replace collision fails with `EEXIST`;
7. sync the probe directory again;
8. reopen descriptor-relatively and verify the bytes;
9. create and rename a child directory;
10. sync the child and parent directories;
11. remove every probe object and sync the destination parent.

Unexpected syscall behavior or success, `EINVAL`, `EROFS`, `EIO`, or cleanup failure returns `unsupported_destination` or `capability_failed`. The probe directory is never reused for staging. File sync alone does not make a directory entry durable, so every named directory sync is required. See [fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html).

## Path and content safety

- Resolve every component with no symlink or magic-link traversal. `openat2` with `RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS` provides this property. After opening the parent, use descriptor-relative operations. See [openat2(2)](https://man7.org/linux/man-pages/man2/openat2.2.html).
- Reject a final or staging path that is the private storage root, an ancestor or descendant of it, inside any Revision/staging/quarantine/candidate/source-backup workspace, or overlapping its own staging.
- Detect bind-mount aliases and opened-path identity relationships; lexical ancestry checks alone are insufficient.
- Reject final or intermediate symlinks, non-directory ancestors, mount replacement, and bind aliases that evade lexical checks.
- Copy only the exact Revision IDs in the snapshot and lease. Never recursively select backup content by walking the storage root.
- Never follow links inside a Revision or bundle. Unexpected symlinks, hard links, devices, sockets, FIFOs, mount points, duplicate names, or unmanifested members fail verification.
- Create destination members as new regular files and directories with effective modes `0600` and `0700`, regardless of hostile umask. Logical bytes and digests are authoritative; ownership, ACLs, xattrs, hard links, reflinks, sparse layout, and source modes are not preserved.
- Use a checked read/write loop for transport. `copy_file_range` is optional acceleration only with fallback, exact length checks, and full digest verification. See [copy_file_range(2)](https://man7.org/linux/man-pages/man2/copy_file_range.2.html).

## Bundle format and binding

A finalized bundle contains:

```text
catalogue.sqlite
revisions/
  <revision-id>/
    <served members>
    <reserved Revision recovery manifest>
backup-manifest.json
.observatory-complete
```

### SQLite snapshot and exact lease

Create the catalogue snapshot with SQLite's Online Backup API, never by copying the main database file in WAL mode. An unfinished Online Backup destination transaction rolls back, while a completed snapshot includes committed WAL state. See [SQLite Online Backup API](https://sqlite.org/backup.html), [backup C API](https://sqlite.org/c3ref/backup_finish.html), and [WAL](https://sqlite.org/wal.html).

After completion, close and sync the snapshot, require no necessary `-wal` or `-shm` sidecar, reopen read-only, verify Observatory `application_id`, supported schema, `quick_check`, and `foreign_key_check`, then record its exact length and digest.

The snapshot/lease race protocol is:

1. Commit the authoritative `storage_operations` intent before external effects.
2. Acquire a short application barrier against catalogue mutations that create, remove, quarantine, or change Revision availability.
3. Produce the Online Backup snapshot under `backups/<backup-id>/`.
4. Query that completed snapshot for the exact ordered set of committed, available Revision IDs and expected Revision-manifest digests.
5. In one live-catalogue transaction, recheck every Revision and create the authoritative lease over exactly that set.
6. When any Revision changed, discard only the unexported operation-owned snapshot and retry from the barrier. Do not create destination staging.
7. Release the barrier only after the exact lease commits.

Ordinary metadata may change after the snapshot. The backup remains the point-in-time catalogue represented by that snapshot; immutable leased Revisions cannot be cleaned during copy.

### Top-level manifest

`backup-manifest.json` is versioned canonical data containing at least:

- backup format version, backup ID, and digest algorithm;
- Observatory application ID, catalogue schema/user version, creator build, and API format version;
- creation and snapshot instants;
- catalogue length and digest;
- the exact ordered Revision set;
- for each Revision, Artifact ID, Revision ID, Revision-manifest digest, logical file/byte counts, and ordered member path/size/digest records;
- aggregate logical bytes, stored bytes, and file count; and
- verification result and instant.

The manifest excludes source storage paths, Project source paths, absolute destination paths, mount sources, inode/device values, owners, modes, teardown argv, and secret-bearing URLs.

The manifest's Revision set must equal all four of:

1. the backed-up catalogue's committed available set;
2. the authoritative lease set;
3. the destination `revisions/` directory set; and
4. every referenced Revision manifest's identity and digest.

Missing, extra, duplicate, reordered where order is significant, or mismatched entries fail creation.

Backup format v1 implementation must choose one fixed canonical manifest serialization and digest procedure before it can emit v1, publish fixtures for it, and test byte-for-byte interoperability. That serialization is part of format v1 compatibility, not a remaining planning decision or an implementation-selectable behavior after release.

### Completion marker

`.observatory-complete` is written last with no-replace semantics. Its canonical data contains only:

- format version;
- backup ID;
- top-level manifest digest;
- catalogue digest; and
- completion instant.

The marker is evidence, not authority and not a substitute for the manifest. A valid backup requires the final name, a regular marker file, matching marker/manifest/catalogue identities and digests, valid catalogue identity and checks, and the exact safe Revision inventory.

## Copy, verification, sync, and publication

1. **Intent:** commit the operation/backup rows, request fingerprint, redacted destination locator, expected parent and mount fingerprint, phase, record version, and cancellation state.
2. **Snapshot and lease:** create and sync the snapshot; derive, recheck, and lease its exact Revision set.
3. **Capacity:** prove source and destination requirements and reserves.
4. **Probe:** classify and actively test the destination parent.
5. **Stage:** create the hidden sibling directory with no-replace semantics.
6. **Catalogue:** copy, hash, sync, reopen, and validate destination `catalogue.sqlite`.
7. **Revisions:** create members descriptor-relatively in manifest order. Copy while hashing, check length and digest, sync each file, and reject any source identity, size, or manifest change.
8. **Directory durability:** sync Revision directories bottom-up, then `revisions/`, then the staging root.
9. **Manifest:** create and sync `backup-manifest.json`, then sync the staging root.
10. **Deep creation verification:** reread the destination catalogue, every Revision manifest, and every content file; reconcile exact sets, types, sizes, identities, and digests.
11. **Marker:** create and sync `.observatory-complete`, then sync the staging root.
12. **Finalize:** recheck final-path absence and parent/mount fingerprint, then call `renameat2(staging, DESTINATION, RENAME_NOREPLACE)`.
13. **Publish durably:** sync the destination parent.
14. **Complete authoritatively:** in SQLite mark the backup completed, append audit data with manifest/catalogue digests and byte counts, and release the lease.
15. **Clean source evidence:** asynchronously remove expendable snapshot/workspace bytes and sync `backups/`. Cleanup failure is recorded separately and does not invalidate the durable external backup.

Success is returned only after step 14. A client disconnect never cancels server work.

## Crash guarantees

| Crash or fault point | Durable observable outcome | Restart action |
| --- | --- | --- |
| Before intent commit | No operation and no external output | An identical request starts once. |
| After intent, before snapshot | Nonterminal SQLite operation; no valid backup | Resume snapshot or cancel. |
| During Online Backup | Incomplete source snapshot evidence | Remove/recreate only operation-owned snapshot evidence and resume. |
| Snapshot complete, before lease | No external copy; snapshot is evidence only | Revalidate and lease the exact set or regenerate. |
| Lease committed, before staging | Exact Revisions remain protected | Resume or cancel and release after quiescence. |
| During destination copy | Hidden incomplete directory only | Hash completed members before skipping; recreate partial or mismatched members. |
| After payload sync, before marker | Hidden incomplete directory only | Deep-verify, then write marker. |
| After marker sync, before rename | Hidden staging is still invalid by location | Recheck parent and collision, then rename. |
| During final rename | Either hidden staging or one final directory; never half a rename | Match SQLite intent, IDs, digests, and descriptors; never infer from presence alone. |
| After rename, before parent sync | Final name may not survive power loss; success was not returned | Reconcile staging/final evidence; sync and continue only when exact identity matches. |
| After parent sync, before SQLite completion | External backup is independently complete; lease may remain active | Verify exact identity/digests, commit completion, and release lease. |
| After SQLite completion, before workspace cleanup | Backup is valid; redundant source evidence remains | Clean source workspace idempotently. |
| Destination disappears | No valid backup; live authority is unchanged; lease remains | Return retryable `destination_unavailable`; resume only on the same parent/mount fingerprint. |
| Verification mismatch | No final rename; operation records failure | Recopy a proven transport error or require explicit cancel/discard for persistent mismatch. |
| `ENOSPC`, quota, or I/O error | No valid final backup; source authority is unchanged | Report redacted capacity evidence and retain resumable state under policy. |
| Final destination appears concurrently | Competing inode remains untouched | Return `destination_exists`; operator chooses another exact destination. |

Path absence never proves completion. A final path never authorizes completion without the matching authoritative operation, backup ID, destination fingerprint, manifest digest, and catalogue digest.

## Resume, cancellation, and retention

### Resume

```text
obs system recovery resume <backup-id>
```

Resume accepts only an authoritative nonterminal operation. It reopens the parent without following links and matches the operation/request fingerprint, mount and parent identity, staging evidence, backup ID, snapshot digest, lease set, and expected manifest. It rehashes every allegedly completed destination file before skipping it and recreates partial or mismatched files. An identical create replay can also resume the operation.

Client timeout, terminal closure, and HTTP disconnect do not cancel work. They report unknown completion and direct an identical retry with the same idempotency key. SIGTERM stops dispatch at a safe boundary and leaves resumable authoritative state; SIGKILL and power loss use the crash table.

### Cancellation

```text
obs system backup cancel BACKUP --yes
```

The API is:

```http
POST /api/v1/system/backups/{backupId}/cancel
If-Match: "rv-<current>"
Idempotency-Key: <key>
```

Cancellation of a nonterminal backup requires the current operation ETag/record version. Missing `If-Match` returns `428 precondition_required`; a stale value returns `412 changed_record`. The request records `cancel_requested`, stops before the next file or finalization boundary, waits for an active file write to quiesce, records `cancelled`, releases the lease, and retains exact incomplete evidence for controlled cleanup.

A completed backup cannot be cancelled: a cancel request against an already completed record returns `409 backup_completed` and leaves the bundle untouched. When an accepted cancellation races with finalization and durable final rename plus parent sync wins, the operation resolves as `completed` and returns that terminal representation. There is no transition from `completed` to `cancelled`.

### Twenty-four-hour incomplete retention

Active and resumable operations retain their source workspace, lease, and known destination staging for 24 hours after the last durable progress commit. Each durable progress commit restarts the interval.

After expiry, a reconciler can mark a nonterminal operation `abandoned` and release its lease only after proving no worker or finalization is active. Source-side abandoned work is first in pressure-cleanup order. Destination staging can be auto-deleted only when the authoritative operation and all operation-owned evidence agree on the backup ID, parent fingerprint, and expected staging name and no final backup exists.

Any identity, fingerprint, ownership, or state ambiguity becomes `stale_ambiguous`. Ambiguous external staging is never auto-deleted. Cancelled, failed, abandoned, and ambiguous incomplete output remains selectable by exact recovery `discard`. A final backup is operator-owned and never subject to Observatory retention or cleanup.

## Capacity accounting

### Source filesystem

Preflight reports and accounts for:

- the SQLite snapshot upper bound from page count/page size plus header and temporary overhead;
- authoritative operation rows and source workspace/receipt evidence;
- existing source workspace, staging, and quarantine bytes;
- `statvfs.f_bavail` free bytes;
- the configured ceiling; and
- Observatory's greater-of-1-GiB-or-5% operational reserve.

The source must satisfy:

```text
available_after_required_source_bytes >= Observatory source reserve
```

Revision payload bytes are not duplicated in the source workspace.

### Destination filesystem

Required bytes include the catalogue snapshot, exact logical Revision bytes and manifests, top-level manifest, completion marker, directory/allocation overhead estimate, and reusable verified bytes already in matching resumable staging. Preserve a destination reserve equal to the greater of 1 GiB or 5% of filesystem capacity.

Report logical required bytes, `f_bavail` evidence, reserve, and already-staged bytes. Continue handling quota, compression, COW, and allocation failures because `statvfs` cannot guarantee allocation. See [statvfs(3)](https://man7.org/linux/man-pages/man3/statvfs.3.html).

Restore separately budgets the Observatory-owned candidate copy, live authority, rollback catalogue and displaced bytes, and normal source reserve. The external read source needs no write capacity.

## Verification and privacy

```text
obs system backup verify BACKUP [--deep]
```

`BACKUP` accepts an absolute finalized bundle path or a known backup ID whose terminal SQLite record retains a usable private locator. It never accepts `.incomplete` staging.

Normal verification checks safe local filesystem/path identity, final name and marker, marker/manifest/catalogue binding, manifest schema, catalogue application/schema identity, `quick_check`, `foreign_key_check`, exact Revision inventory, each Revision-manifest digest, declared types and sizes, and the absence of unsafe extras.

`--deep` adds SQLite `integrity_check`, a complete content rehash, and full catalogue-to-manifest-to-content reconciliation. Creation always performs the deep content verification before publication. A database-only input returns `catalogue_only`, never a valid backup.

Privacy rules:

- Absolute storage, Project, home, source Revision, destination, and mount-source paths never enter manifests, audit, logs, diagnostics, error details, or remote API results.
- The daemon privately retains the destination locator only while required for resume or verify-by-ID. Long-lived records retain backup ID, operator-supplied basename, filesystem class and nonportable mount fingerprint, digests, counts, bytes, state, and timestamps.
- The server never returns an absolute destination path remotely. API output uses `destination: {"basename":"...","redacted":true}`.
- A local CLI may echo only the exact `DESTINATION` argument that local caller already supplied. It does not learn a path from the server.
- Safe relative manifest members may appear in mismatch details after bounding and truncation. They are never joined to a private absolute root.
- URLs, teardown argv, SQLite row/page contents, file contents, ownership, inode/device values, and mount source remain redacted.
- Backup directories and files use `0700` and `0600`. A bundle contains private recovery data, not a portable public Artifact.

## Restore intake into Observatory-owned staging

```text
obs system recovery preview restore_backup BACKUP
obs system recovery apply PLAN --yes
```

Preview resolves an external finalized bundle without symlinks, requires a supported read-capable host-local filesystem, rejects remote/FUSE input, `.incomplete`, missing markers, unsupported schemas, unsafe members, storage overlap, and changing mount/path identity, then runs deep verification. It reports exact loss, conflict, and capacity effects against current authority. Read-only verification does not lease live Revisions.

Apply acquires the offline/global maintenance gate and records authoritative intent before copying. It then:

1. creates `staging/<operation-id>/` on Observatory's private filesystem;
2. copies the verified external catalogue and Revisions into that staging directory;
3. verifies every copied byte again;
4. constructs candidate metadata and the exact loss report;
5. syncs every candidate file and directory;
6. atomically renames staging to `candidates/<candidate-id>/` on the same private filesystem;
7. syncs `staging/` and `candidates/`; and
8. validates the candidate under issue #16's full rules.

Observatory never serves from, bind-mounts, hard-links, reflinks, or retains a live dependency on the external bundle. Candidate activation remains a separate exact preview/apply operation under the offline gate: install complete candidate bytes first, select catalogue authority last, retain former authority as rollback material, never merge or renumber conflicts, and expose only old or new complete authority after a crash.

## Public CLI and API contract

The complete backup surface is:

```text
obs system backup create DESTINATION
obs system backup verify BACKUP [--deep]
obs system backup cancel BACKUP --yes
obs system recovery resume [BACKUP]
obs system recovery preview restore_backup BACKUP
obs system recovery apply PLAN --yes
obs system recovery preview discard BACKUP
```

The API surface is:

```text
POST /api/v1/system/backups
GET  /api/v1/system/backups/{backupId}
POST /api/v1/system/backups/{backupId}/cancel
POST /api/v1/system/backups/verify
```

Create and cancel are mutations. They require `Idempotency-Key`; cancel also requires the current `If-Match`. Collection create does not require `If-Match`. Replay, fingerprint, timeout, stdout/stderr, HTTP status, CLI exit, and `{schemaVersion,ok,result|error}` envelope behavior follows issue #17 without a backup-specific variant.

A durable nonterminal create returns `202` with an operation representation and status URL. `GET /api/v1/system/backups/{backupId}` returns the current representation and strong operation ETag. A completed create or terminal replay returns the terminal representation. The server redacts the destination path in every case.

Example status result:

```json
{
  "schemaVersion": 1,
  "ok": true,
  "result": {
    "backupId": "6r3t8w2y5b9c4d7f0g1h3j5k7m",
    "recordVersion": 7,
    "state": "copying",
    "phase": "revision_content",
    "filesCompleted": 82,
    "filesTotal": 143,
    "bytesCompleted": 104857600,
    "bytesTotal": 184224912,
    "resumable": true,
    "incompleteExpiresAt": "2026-07-11T03:00:00Z",
    "destination": {"basename": "observatory-2026-07-10", "redacted": true}
  }
}
```

Stable operation states are:

```text
intent_recorded
snapshotting
leasing
probing_destination
copying
verifying
completion_marker_synced
finalizing
completed
cancel_requested
cancelled
failed_resumable
failed_terminal
abandoned
stale_ambiguous
```

Backup-specific errors extend the control-plane taxonomy:

| Code | CLI exit | Meaning |
| --- | ---: | --- |
| `unsafe_destination` | 2 | Invalid, symlinked, overlapping, or recursive path |
| `unsupported_destination` | 2 | Filesystem outside the supported local allowlist |
| `capability_failed` | 10 | Required durability probe failed |
| `destination_exists` | 4 | Strict final-path collision |
| `staging_collision` | 4 | Nonmatching staging object exists |
| `destination_changed` | 4 | Parent, mount, or path fingerprint changed |
| `destination_unavailable` | 5 | Destination disappeared or became inaccessible |
| `contention` | 6 | Lease, maintenance, or catalogue contention exhausted |
| `capacity` | 10 | Source or destination reserve/capacity failure |
| `verification_failed` | 10 | Snapshot, inventory, manifest, or content mismatch |
| `backup_incomplete` | 10 | Verify/restore input lacks valid completion |
| `catalogue_only` | 10 | SQLite file is not a complete backup |
| `backup_completed` | 4 | A completed backup cannot be cancelled |
| `cancelled` | 0 | Cancellation reached a safe terminal state |

`precondition_required`, `changed_record`, `idempotency_conflict`, `idempotency_in_progress`, and `client_timeout` retain issue #17's exact HTTP/CLI meanings. Capacity errors identify `filesystem`, `requiredBytes`, `availableBytes`, `reserveBytes`, and `alreadyStagedBytes` without a path. Collision errors return only the safe basename. Verification errors may include backup/Revision IDs and a bounded safe relative member.

Human success uses stdout and progress/warnings use stderr. JSON success is one stdout envelope. JSON command failure leaves stdout empty and writes one stderr error envelope.

## Acceptance and fault tests

### Destination and path

1. Accept btrfs, ext4, and XFS only after the active probe.
2. Reject NFS, CIFS, FUSE, 9p, virtiofs, overlayfs, tmpfs, read-only mounts, and unknown types even when individual calls appear to work.
3. Inject failure into every probe syscall and cleanup step.
4. Prove `RENAME_NOREPLACE` never overwrites any inode type.
5. Replace or unmount/remount the destination after preflight and require `destination_changed`.
6. Exercise bind mounts and lexical aliases to prove private-root overlap rejection.
7. Swap symlinks and magic links in every path component.
8. Race final-leaf creation against final rename.
9. Verify `0700`/`0600` modes under hostile umask.

### Snapshot, authority, and lease

10. Concurrently publish, replace, quarantine, and clean while snapshotting; prove snapshot set equals authoritative lease set and manifest set.
11. Crash at each snapshot/lease transaction boundary.
12. Inject `SQLITE_BUSY`, WAL growth, checkpoint contention, `SQLITE_IOERR`, and backup restart.
13. Prove a copied WAL-mode main database alone is `catalogue_only` or inconsistent, never complete.
14. Attempt cleanup of leased and unleased Revisions; block only the exact lease set.
15. Reconcile active, completed, missing, cancelled, expired, and ambiguous lease owners.
16. Corrupt or replace `operation.json`, identity evidence, and receipts; prove SQLite remains sole authority and ambiguity never changes state or deletes bytes.

### Copy, verification, and format

17. Fault after every file create, write, digest, file sync, directory sync, manifest write, marker write, rename, and parent sync.
18. Inject short writes, `EINTR`, `ENOSPC`, quota failures, `EIO`, read errors, and destination removal.
19. Corrupt the catalogue, Revision manifest, content, top-level manifest, and completion marker independently.
20. Add, remove, rename, link, or replace destination members during verification.
21. Prove no partial or hidden directory passes verify or restore.
22. Prove creation fully rehashes destination content before finalization.
23. Cover sparse input, zero-length, large, Unicode, dotfile, deep-tree, and maximum-name members without path reinterpretation.
24. Prove source Revision mutation or identity change blocks completion despite catalogue claims.
25. Freeze format-v1 canonical serialization fixtures; require byte-identical manifests and digests across supported implementations and reject noncanonical or ambiguously encoded input where v1 requires canonical bytes.

### Finalization, restart, and idempotency

26. Simulate SIGKILL/power faults after every crash-table phase.
27. Prove the only visible outcomes are no final bundle or one complete final bundle.
28. Crash after rename but before parent sync and permit only evidence-based resume.
29. Crash after parent sync but before SQLite completion; reconcile completion and release the lease.
30. Create the final destination concurrently and prove no overwrite.
31. Replay the same idempotency key at every phase without repeating terminal effects.
32. Reuse a key with a changed destination or options and require `idempotency_conflict`.

### Cancel, resume, and retention

33. Cancel before snapshot, during copy, during verification, before rename, and after durable finalization.
34. Require the current ETag for every nonterminal cancellation; test missing and stale preconditions.
35. Prove completed backups cannot be cancelled or deleted by cancel.
36. Prove client disconnect does not imply cancellation and SIGTERM leaves a resumable intent and protected lease.
37. Resume only the exact SQLite operation after matching and rehashing source and destination evidence.
38. Expire incomplete work after 24 hours; release the lease only after proving no worker or finalization.
39. Modify or replace external staging before cleanup; require `stale_ambiguous` and no automatic deletion.
40. Prove pressure cleanup counts source snapshot/workspace but never deletes a final external backup.

### Capacity and restore

41. Exhaust source and destination independently.
42. Test one byte below, equal to, and above each greater-of-1-GiB-or-5% reserve boundary.
43. Inject destination quota failure despite favorable `statvfs`.
44. Resume partial copy and prove remaining-capacity accounting counts only verified reusable staging bytes.
45. Budget restore with live authority, complete candidate, and rollback material coexisting.
46. Reject unsupported, symlinked, incomplete, changed, remote/FUSE, or overlapping restore input.
47. Deep-verify external input before candidate creation and reverify the private candidate copy.
48. Crash after every candidate file/directory sync and rename.
49. Prove no external byte is served, linked, reflinked, or selected as authority.
50. Detect ID conflicts and post-backup loss exactly; never merge or renumber.
51. Crash before and after authority selection and retain only old or new complete authority.
52. Validate explicit rollback from retained former authority.

### API, CLI, and privacy

53. Exercise create, get, verify, resume, discard, and cancel in human and JSON modes.
54. Snapshot every state, error, HTTP status, CLI exit, ETag, replay header, and timeout result against issue #17 envelopes.
55. Assert remote responses never contain absolute destination paths; prove local CLI echoes only its own input argument.
56. Assert logs, audit, diagnostics, manifests, and errors contain no storage, home, Project, Revision-source, destination, or mount-source absolute paths.
57. Assert only safe basename and bounded relative member reporting.
58. Verify stdout/stderr separation, one-envelope JSON, progress behavior, and ordered fault results.
59. Verify the manifest excludes paths, inode/device values, ownership, teardown argv, and secret-bearing URLs.

## Sources

- [Issue #18](https://github.com/Whamp/observatory/issues/18) — decision scope.
- [Issue #7 resolution](https://github.com/Whamp/observatory/issues/7#issuecomment-4930161768) — reserve, pressure cleanup, retention, and failure behavior.
- [Issue #11 resolution](https://github.com/Whamp/observatory/issues/11#issuecomment-4930220621) — SQLite authority, immutable Revisions, leases, and intent ordering.
- [Issue #16 resolution](https://github.com/Whamp/observatory/issues/16#issuecomment-4931158568) — candidate restore and recovery boundaries.
- [Issue #17 resolution](https://github.com/Whamp/observatory/issues/17#issuecomment-4931421145) — public API/CLI, envelopes, ETags, preconditions, idempotency, and timeout behavior.
- [rename(2)](https://man7.org/linux/man-pages/man2/rename.2.html) — same-filesystem atomic rename and `RENAME_NOREPLACE`.
- [fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html) — file and containing-directory durability.
- [openat2(2)](https://man7.org/linux/man-pages/man2/openat2.2.html) — no-symlink descriptor-relative traversal.
- [statfs(2)](https://man7.org/linux/man-pages/man2/statfs.2.html), [statx(2)](https://man7.org/linux/man-pages/man2/statx.2.html), and [mountinfo(5)](https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html) — filesystem and mount identity.
- [SQLite Online Backup API](https://sqlite.org/backup.html) and [SQLite WAL](https://sqlite.org/wal.html) — consistent live snapshot semantics.
- [RFC 9110 §13](https://www.rfc-editor.org/rfc/rfc9110#section-13), [RFC 6585 §3](https://www.rfc-editor.org/rfc/rfc6585#section-3), and [IETF Idempotency-Key draft §2](https://datatracker.ietf.org/doc/html/draft-ietf-httpapi-idempotency-key-header#section-2) — public precondition and idempotency semantics inherited from issue #17.

## Residual implementation risk

The active probe demonstrates behavior under the running kernel, filesystem, and mount configuration; it does not prove every storage device's power-loss behavior. Implementation acceptance therefore needs block-device fault injection and real power-cut evidence on each declared deployment filesystem. Adding any filesystem beyond btrfs, ext4, and XFS requires a new evidence matrix and an explicit product decision.
