# Observatory product specification

Status: **decision-locked planning specification**
Canonical specification: this file
Canonical deployment origin: `https://desktop.greyhound-chinstrap.ts.net/`
Planning map: [issue #1](https://github.com/Whamp/observatory/issues/1)
Specification assembly: [issue #13](https://github.com/Whamp/observatory/issues/13)

## Contents

- [1. Purpose, authority, and normative language](#1-purpose-authority-and-normative-language)
- [2. Scope and planning boundary](#2-scope-and-planning-boundary)
- [3. Domain model and authority](#3-domain-model-and-authority)
- [4. Project identity and lifecycle](#4-project-identity-and-lifecycle)
- [5. Artifact contract](#5-artifact-contract)
- [6. Service contract](#6-service-contract)
- [7. Routes and public API](#7-routes-and-public-api)
- [8. Agent CLI contract](#8-agent-cli-contract)
- [9. Project-led browser interface](#9-project-led-browser-interface)
- [10. Artifact retention and capacity](#10-artifact-retention-and-capacity)
- [11. Service reachability and expiry](#11-service-reachability-and-expiry)
- [12. Persistence, backup, and crash protocols](#12-persistence-backup-and-crash-protocols)
- [13. Storage diagnostics and recovery](#13-storage-diagnostics-and-recovery)
- [14. Network and security boundary](#14-network-and-security-boundary)
- [15. Implementation, bootstrap, and supervision](#15-implementation-bootstrap-and-supervision)
- [16. Configuration, defaults, errors, and health](#16-configuration-defaults-errors-and-health)
- [17. Implementation acceptance criteria](#17-implementation-acceptance-criteria)
- [18. Deferred work and unsupported configurations](#18-deferred-work-and-unsupported-configurations)
- [19. Install-time agent onboarding text](#19-install-time-agent-onboarding-text)
- [20. Decision traceability](#20-decision-traceability)

## 1. Purpose, authority, and normative language

Observatory is the single known starting point for browser-based work produced or used by AI agents. It makes persistent static Artifacts and separately running browser Services findable from one private, tailnet-only front door.

This file is authoritative for Observatory product and implementation behavior. It synthesizes every closed decision in the [traceability matrix](#20-decision-traceability). Linked issues and research notes retain detailed rationale and evidence; they do not override this specification. Any later conflict with a MUST, MUST NOT, SHOULD, SHOULD NOT, or MAY here requires an explicit product decision and a specification update.

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are normative. Unqualified present-tense statements are requirements when they define behavior. Server-returned URLs are authoritative even when an example shows their shape.

Production implementation and deployment are outside this planning map. A later implementation MUST satisfy this specification and [section 17](#17-implementation-acceptance-criteria). This planning effort does not build, install, approve, or deploy Observatory.

## 2. Scope and planning boundary

### In scope

Observatory specifies:

- an owned catalogue of static Artifacts and immutable Revisions;
- a link-first catalogue of externally owned Services and Targets;
- canonical browser routes and one versioned `/api/v1` application control plane;
- an agent-first resource-oriented `obs` CLI;
- a Project-led browser ledger with secondary search and explicit controls;
- retention, reachability probes, cleanup, diagnostics, backup, and recovery;
- SQLite-authoritative local persistence and crash protocols;
- a private Tailscale Serve deployment with least-privilege grants; and
- one Rust binary, embedded frontend, XDG layout, capability-allowlisted bootstrap, package activation, and systemd user supervision.

### Explicit non-goals

Observatory MUST NOT:

- reimplement, embed, absorb, rewrite, or proxy an external Service's UI, application logic, API, behavior, or state;
- discover Services from processes, listeners, ports, DNS, or network scans;
- start, restart, kill, signal, allocate ports for, supervise, or automatically decommission external Services;
- convert pi-annotate or Sideshow state into Artifacts or provide producer-specific snapshot/conversion adapters;
- act as a generic file browser, source-tree browser, directory listing, SPA fallback host, or source watcher;
- expose public-internet access, Tailscale Funnel, or LAN-only/non-tailnet compatibility;
- provide accounts, application login, API keys, sessions, read/write roles, or per-Entry authorization;
- provide multi-host replication, distributed storage, or external database/search infrastructure; or
- implement or deploy production as part of issue #1.

The complete deferred and unsupported boundary is in [section 18](#18-deferred-work-and-unsupported-configurations).

## 3. Domain model and authority

The terms below are exact and follow [`CONTEXT.md`](CONTEXT.md).

- **Entry**: a named browser destination discoverable through Observatory. An Entry is exactly one Artifact or one Service.
- **Artifact**: a static, persistent, browser-viewable bundle owned by Observatory. It is one regular file or one directory tree of regular files. Its stable identity selects one current Revision while live.
- **Revision**: an immutable published state of an Artifact. A successful replacement creates a Revision and atomically advances the Artifact's current selection.
- **Project**: a work context rooted at a canonical directory. The canonical directory is Project identity. A public ID/key addresses it but does not replace that identity claim.
- **Service**: a separately running interactive browser application referenced by Observatory while retaining its own behavior and state. Its identity is its name within one Project.
- **Target**: a named absolute browser URL through which a Service may be reached. Target names are unique within a Service. Every Service has exactly one primary Target and zero or more alternatives.
- **Teardown Action**: an optional Project-supplied executable and argument list that decommissions a Service only when explicitly requested.
- **Publish**: make an Artifact part of Observatory's owned collection under a stable identity.

Authority is split exactly:

- The daemon application service is the sole domain authority. The CLI, browser UI, and scheduled workers call it and MUST NOT reimplement lifecycle rules.
- SQLite is the sole catalogue authority for identity, metadata, current Revision selection, lifecycle, Services, Targets, observations, tombstones, intents, backup leases, setup-independent operations, and audit events.
- Observatory owns copied Artifact bytes and their lifecycle.
- An external Service owns its process, behavior, state, exposure, TLS, authorization, and normal lifecycle.
- Tailscale grants authorize the Observatory origin and do not transfer authorization to Service Targets.
- Recovery manifests, filesystem presence, workspace receipts, completion markers, and SQLite recovery output are evidence only. They never make an Entry or operation live.
- The invoking verified release-bundle process has narrow local bootstrap authority only for the eight setup/service leaves in [section 15](#local-bootstrap-authority). It MUST NOT access domain or storage authority.

## 4. Project identity and lifecycle

A Project is identified by its daemon-resolved canonical directory. `-p, --project PATH` selects Project context and defaults to the invocation's current working directory. Selection does not allocate identity.

### Resolve and register

`project resolve [PATH]` and `GET /api/v1/projects/resolve?path=…` are read-only. They require an existing accessible directory, canonicalize once, allocate no ID, and return:

- `registered` plus the existing Project reference;
- `unregistered` with no Project reference; or
- `gone` when that canonical directory has a terminal identity.

`project register [PATH]` and `POST /api/v1/projects` are the sole simple allocation operations. Registration canonicalizes the directory, rejects nonexistent/non-directory/inaccessible paths, conflicts when a live Project owns it, returns `410 project_gone` for a tombstoned directory, allocates a random Project ID and stable creation slug, and commits before Service registration can select it. Other create operations MUST return `project_not_registered`; they MUST NOT hide Project allocation inside a read or another create.

The public Project key is `<project-slug>~<project-id>`. Project IDs are caller-independent, never reassigned, and never reused. Title and slug are mutable presentation metadata. `PATCH /api/v1/projects/{projectId}` and `obs project update` may change only title or slug.

### Explicit move

`project move PROJECT NEW_PATH --yes` and `POST /api/v1/projects/{oldProjectId}/move` perform one atomic catalogue transition. The request MUST carry the old Project `If-Match` and enumerate every live Service exactly once with its expected record version. The daemon MUST:

1. canonicalize `NEW_PATH` and require it to be unregistered and never tombstoned;
2. verify the complete Service enumeration and every version;
3. allocate a new Project ID and a new ID for each re-registered Service;
4. copy each Service's configuration, Targets, primary choice, pin metadata, and Teardown Action;
5. reset every new Target observation to `unknown` and queue probes;
6. tombstone every old Service ID;
7. tombstone the old Project ID with cause `moved`;
8. leave every Artifact associated with the old tombstoned Project;
9. leave every Artifact serving URL unchanged; and
10. return ordered old-to-new Project and Service mappings.

A missing/extra/changed Service, path conflict, validation failure, or authority gate aborts the entire move. Move does not redirect old IDs, infer filesystem movement, execute teardown, reassociate Artifacts, or affect external processes.

### Tombstone

`project tombstone PROJECT --yes` and `DELETE /api/v1/projects/{projectId}` require matching record version, zero live Services, and no nonterminal Project-scoped operation. Artifacts remain associated and continue independently. The Project then returns `410 Gone` permanently without redirect or ID reuse.

## 5. Artifact contract

### Publish shapes, entry point, and media

Publish accepts exactly:

1. one regular file, which is its own entry point; or
2. one directory tree of regular files, which is one Artifact.

Directory entry precedence is:

1. request option;
2. `entry` in root `.obs.json`;
3. root `index.html`;
4. otherwise failure.

The entry path MUST be relative, remain inside the root, and name a regular file. Valid entry media types are `text/*`, `image/*`, `audio/*`, `video/*`, PDF, and JSON. Markdown is served as text. CSS, JavaScript, fonts, archives, and arbitrary binaries may support an entry but MUST NOT be entries. MIME comes from the filename and every byte response includes `X-Content-Type-Options: nosniff`.

Observatory copies and serves bytes without compilation, rendering, transformation, fetching, inlining, or URL rewriting. Internal references SHOULD be relative. Root-relative references are unsupported and detectable instances SHOULD warn. Absolute external URLs remain network-dependent. An Artifact/Revision base serves its entry; a suffix MUST name an actual file. There is no SPA fallback or directory listing. Hash routing remains available.

### Portable metadata

Root `.obs.json` is optional and recognized only for directory Artifacts:

```json
{
  "schemaVersion": 1,
  "entry": "artifact.html",
  "title": "Authentication flow",
  "description": "Review of the proposed sign-in architecture"
}
```

Only `schemaVersion`, `entry`, `title`, and `description` are portable. `.obs.json` is consumed and not served. Project, slug, publication time, stable identity, retention, and Revision history belong to SQLite. Equivalent single-file metadata comes from options. Title precedence is option, `.obs.json`, HTML `<title>`, source basename. Description precedence is option, `.obs.json`, empty.

### Validation and ownership

Publish rejects symlinks, multiple-link-count files, sockets, devices, FIFOs, absolute/traversal paths, unreadable members, anything outside the root, and non-regular leaves. Sources are opened descriptor-relatively without following links. Every regular file, including dotfiles, is copied except root `.obs.json`; there are no implicit ignores.

Missing/invalid entry, malformed metadata, unsafe content, unreadable bytes, or unsupported entry media blocks Publish. Missing references, root-relative URLs, unreachable dependencies, and browser/JavaScript failures may warn but do not block. Observatory does not execute an Artifact to prove it renders.

There is no default file-count or per-Artifact byte limit. Capacity and configured ceilings fail clearly before visibility. Artifacts are trusted single-user content; JavaScript runs normally on the Observatory origin without per-Artifact isolation.

### Create, replace, and Revision semantics

`artifact publish` is strict creation and allocates never-reused Artifact/Revision IDs. Identity is never inferred from title, filename, source path, slug, bytes, or metadata.

`artifact replace ARTIFACT SOURCE` explicitly names a live Artifact, creates an immutable Revision, and atomically advances current selection. Prior Revisions remain immutable until retention removes them. A failed/interrupted replacement leaves current selection and retention unchanged.

Publish stages, validates, checksums, and durably finalizes a complete owned copy before catalogue visibility. Later source changes do not affect it. No failure exposes a partial bundle.

### Explicit import and per-entry outcomes

`artifact import SOURCE...` is explicit migration intake into normal Publish, never discovery or another Artifact kind.

- Each ordered entry selects one host-local regular file or directory. Relative paths resolve against the caller working directory. URL import, stdin archives, internal globbing, crawling, watch mode, and conversion are prohibited.
- A directory is one Artifact. Publish entry, metadata, validation, MIME, warning, copy, retention, route, and Revision rules apply.
- Request defaults and each per-entry option may set only `projectId`, `entry`, `title`, `description`, `slug`, and `retention`. Per-entry options cannot change source or index.
- Import always copies and never serves through, links, bind-mounts, moves, deletes, or retains a source relationship.
- Copy compares opened identity, type, size, timestamps, and link count before/after each read and retraverses directories. Any addition, removal, rename, or metadata change fails that entry as `source_changed`.
- A fingerprint over entry path plus ordered relative paths, sizes, and digests may identify duplicate candidates but never reuses/replaces identity.
- Repeated normalized selections fail every occurrence as `duplicate_selection`.
- Commit is atomic per entry, not per batch. Bulk idempotency derives per-entry keys as `SHA-256("observatory-import-v1" || 0x00 || request-key || 0x00 || decimal-index)`.
- Each ordered outcome contains index, safe label, `committed|failed|unchanged_replay`, Artifact/Revision IDs when allocated, canonical URLs, logical files/bytes, effective retention, warnings, duplicate candidates, stable error details, and a separate cleanup/quarantine error when cleanup also failed.
- Durable provenance records import method, commit instant, actor/request identity, and content fingerprint, but excludes absolute source paths, home components, device/inode, ownership, and permissions.

Trustworthy mixed and zero-success batch behavior follows [section 7](#batch-and-bulk-results).

## 6. Service contract

### Identity, registration, and update

A Service is identified by `(Project canonical directory, Service name)`. The immutable name is Unicode NFC, nonempty after trimming, control-free, and compared case-sensitively as NFC UTF-8. Project move or Service rename creates a new identity and tombstones the old. Target URL changes do not change identity.

Registration is strict creation and conflicts on an existing identity. It commits without reachability, starts every Target at `unknown`, and queues probes. Update requires a live Service and may atomically change presentation, Targets, or Teardown Action but not identity. Missing update/removal returns not found. Removal deletes catalogue metadata/observations only and does not affect the process. Observatory never discovers Services.

### Targets and Open behavior

Every Service has exactly one primary Target and zero or more alternatives. Each Target has a Service-local unique NFC name, absolute credential-free HTTP(S) URL, and optional label. `local` and `tailnet` are conventions only. Primary means configured Open destination, not health. Alternatives appear in details. Observatory never probe-selects or silently falls back. Removing primary names replacement in the same atomic mutation.

Service representations return distinct `detailUrl` and direct `primaryTargetUrl`. Service Open navigates directly to `primaryTargetUrl`; Observatory does not proxy, rewrite, wrap, or derive it.

### Credential-free Target validation

A Target URL is valid only when:

1. it parses as an absolute URI;
2. normalized scheme is exactly `http` or `https`;
3. authority and nonempty valid IPv4, bracketed IPv6, or IDNA DNS host exist;
4. explicit port is `1..65535`;
5. userinfo is absent, including empty userinfo syntax;
6. fragment is absent;
7. no control, space, CR/LF/tab/NUL, invalid escape, raw backslash, or traversal interpretation exists;
8. stored escapes use uppercase hex; and
9. percent-decoded, ASCII-case-folded query names exclude exactly:

```text
access_token api_key apikey auth authorization bearer credential jwt key
passwd password sig signature token x-amz-credential x-amz-signature
x-goog-credential x-goog-signature
```

Unknown ordinary query names are allowed. Canonical serialization does not probe, rewrite path/query, follow redirects, or change Service identity. Rejected credential material MUST NOT enter logs, diagnostics, or observations.

### Teardown Action

A Service may store one executable plus argv, never a shell string, and a default action timeout. Default is 30,000 ms; accepted range is `1000..300000` ms. An explicit teardown may override the timeout within that range.

Teardown runs only after explicit confirmation in the Project canonical directory. Success requires an action, existing Project directory, argv launch, no timeout, exit `0`, and unchanged record version; only then is the Service tombstoned. Missing directory, launch error, signal, nonzero exit, timeout, or changed record preserves it.

Captured stdout/stderr are each limited to 16 KiB after UTF-8 replacement, control-escaped, truncation-labeled, omitted from ordinary logs, and returned only in the confirmed outcome. A known action failure/timeout is trustworthy `ok:true` and CLI exit `9`; it is not unknown commit. Teardown is the sole external runtime effect and automatic expiry never invokes it.

## 7. Routes and public API

### Namespace allocation and browser routes

| Namespace | Contract |
| --- | --- |
| `/` | Permanent front door; `308 Permanent Redirect` to `/ui/`. |
| `/ui/…` | Human ledger, Project, detail, and allowed control pages. |
| `/api/v1/…` | Versioned JSON application control plane. |
| `/_static/<build-id>/…` | Immutable embedded UI assets. |
| `/artifacts/<artifact-key>/…` | Stable-current Artifact bytes. |
| `/revisions/<revision-id>/…` | Immutable Revision bytes. |

These first segments are reserved. Unknown top-level routes return `404`. `/api` and unversioned API routes do not redirect. There is no generic `/entries` route.

Project, Service, and Artifact route keys are `<slug>~<id>`; Revision routes use only ID. IDs are independently random 128-bit values encoded as exactly 26 lowercase Crockford-base32 characters. They are opaque, caller-independent, collision-retried before visibility, never reassigned, and never reused. Issued terminal IDs retain tombstones and return `410`; unknown IDs return `404`.

Slug normalization is exact: Unicode NFKD; discard combining marks and non-ASCII not transliterated by decomposition; lowercase ASCII; replace maximal non-`a-z0-9` runs with `-`; collapse/trim; truncate to 48 without trailing `-`. Stored grammar is `[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?`. Empty supplied values fail; empty derived values use `project|service|artifact`. Bare-slug collisions are allowed. Explicit rename preserves ID; stale valid slug with a live ID receives one `308` to current key. No aliases bridge IDs.

Artifact suffixes reserve no child segment. Decode once as UTF-8 per segment. Reject malformed/encoded separators, backslash, NUL, `.`/`..`, traversal, duplicate separators, and double decoding. Canonical URLs leave unreserved bytes literal and uppercase-percent-encode all others. Manifest lookup is case-sensitive.

UI collection/detail and Artifact/Revision bases end in `/`; files do not. Safely identifiable GET/HEAD slash, uppercase-ID, stale-slug, or encoded-key canonicalization gets one absolute `308`. API routes are exact and never redirect. Redirects preserve query. Query is not identity/file lookup; fragments are browser semantics.

### Common API contract

All `/api/v1` requests/responses are UTF-8 JSON with `Content-Type: application/json`, `Accept: application/json`, and `Cache-Control: no-store`. Successful or trustworthy outcomes use:

```json
{"schemaVersion":1,"ok":true,"result":{}}
```

Command-level inability uses:

```json
{"schemaVersion":1,"ok":false,"error":{"code":"changed_record","message":"the resource changed","retryable":false,"details":{}}}
```

`ok:true` does not mean every entry succeeded, storage is healthy, a Service is reachable, or teardown exited `0`. `ok:false` means no trustworthy requested outcome could be returned. Unknown enums are never coerced; unknown input fields are ignored only where explicitly extensible.

API instants are RFC 3339 UTC `Z` strings with millisecond precision; durations are integer milliseconds; byte counts/versions are unsigned integers; digest is lowercase `<algorithm>:<hex>`, initially SHA-256.

Every API resource create/get/update/transition and every successful per-entry API outcome returns opaque IDs, current key where applicable, `recordVersion`, `apiUrl`, and all applicable canonical browser URLs. This requirement applies to API resource representations, never byte-serving GETs under `/artifacts/` or `/revisions/`. Clients MUST NOT construct URLs.

Project, Artifact, Service, and Revision path parameters use opaque IDs; Target routes use the exact Target name. Live/tombstoned/unknown/wrong-type/malformed selectors map respectively to success, `410`, `404`, `404`, and `422`. API routes never redirect to repair IDs, keys, slashes, or versions.

Resource schemas are stable:

| Schema | Required fields |
| --- | --- |
| Project | `kind`, `id`, `key`, `recordVersion`, `state`, `title`, `slug`, `canonicalDirectory`, `createdAt`, `updatedAt`, `apiUrl`, `detailUrl`; gone adds `terminalState`, `tombstonedAt`, `cause` |
| Retention | `mode=default|ttl|pinned`, `ttlMs`, `expiresAt`, `pinReason`, `recoveryUntil` |
| Artifact | `kind`, `id`, `key`, `recordVersion`, `state`, `title`, `description`, `slug`, Project reference, `currentRevisionId`, Retention, `files`, `logicalBytes`, `revisionCount`, `publishedAt`, `updatedAt`, `apiUrl`, `openUrl`, `detailUrl` |
| Revision | `kind`, `id`, `artifactId`, `state=current|superseded|unavailable|gone`, `entryPath`, `entryMediaType`, `files`, `logicalBytes`, `manifestDigest`, `publishedAt`, `apiUrl`, `openUrl` |
| Service | `kind`, `id`, `key`, `recordVersion`, `state`, immutable `name`, `label`, `description`, `slug`, Project reference, pin/expiry fields, `primaryTargetName`, `reachability`, redacted teardown availability/timeout, Targets, `apiUrl`, `detailUrl`, `primaryTargetUrl` |
| Target | `name`, `label`, canonical credential-free `url`, `primary`, `targetVersion`, `reachability`, latest observation result/time/duration/status-or-failure/host-vantage |
| Per-entry outcome | `index`, safe `label`, operation-specific terminal `status`, and exactly one of `result` or stable `error` |
| Durable operation | opaque operation/backup/plan ID, `recordVersion`, state/phase/progress, resumability/expiry, redacted locators, and canonical status `apiUrl` |

Ordinary Service representations expose Teardown Action availability and timeout, never executable/argv. Absolute Project `canonicalDirectory` is visible because it is identity; other remote private paths are redacted.

Mutable singleton representations carry strong `ETag: "rv-N"` and matching positive `recordVersion`. PATCH, DELETE, and action POSTs on existing resources require `If-Match`. Target mutations use the containing Service ETag; recovery apply uses the plan/current operation ETag; backup cancel uses the backup operation ETag. Missing is `428 precondition_required`; mismatch is `412 changed_record`; state conflict after a match is `409`; success returns a new ETag/version. Collection strict creation has no `If-Match` and conflicts with `409`. Immutable Revision metadata has no mutation precondition.

Every API mutation requires `Idempotency-Key`. Non-TTY/automation CLI mutations require a caller key. Interactive TTY mutation may generate a cryptographically random key but MUST display it before dispatch. Keys are 8–200 visible ASCII characters with no whitespace, controls, quote, or backslash, compared byte-for-byte deployment-wide and redacted in logs.

Fingerprint is SHA-256 over API version, uppercase method, canonical route, RFC 8785 canonical JSON body, normalized IDs/resolved canonical Project/source paths, `If-Match`, and semantic options. Accept/user-agent/client-timeout/tracing are excluded. Host-source first acceptance also binds the durable intent to the observed snapshot. New key executes; same fingerprint resumes/replays; different fingerprint is `409 idempotency_conflict`; concurrent nonterminal duplicate is retryable `409 idempotency_in_progress` with `Retry-After`. Validation before dispatch consumes no key. Replay sets `Idempotency-Replayed: true` and preserves original semantics, URLs, operation ID, and ETag.

A CLI wait timeout after dispatch leaves stdout empty, emits `ok:false client_timeout` to stderr, exits `5`, states commit is unknown, and directs identical retry with the same key. Disconnect never cancels durable work.

### Pagination and search

Every unbounded collection accepts `limit=50` (`1..200`), `after=<opaque>`, `order=<enum>`, and `direction=asc|desc`. Responses contain ordered `items` and `{limit,nextCursor,hasMore}` plus an absolute RFC 8288 `rel="next"` Link when needed. Integrity-protected cursors bind endpoint/filter/order and expire after 15 minutes; malformed/mismatched is `422 invalid_cursor`, expired is `409 cursor_expired`. ID is final ascending tie-breaker.

Projects filter `state|query`; Artifacts `projectId|state|retentionMode|query`; Services `projectId|reachability|pinned|query`; Revisions `availability`; ledger `projectId|kind|query`; audit `resourceType|resourceId|cause|actor|since|until`. Orders are the endpoint-appropriate `recent|title|attention|published|superseded|timestamp`. Search is literal Unicode-aware case-folded substring matching, not relevance-ranked full-text search.

### Exact `/api/v1` inventory

#### Projects and ledger

| Method | Path | Operation |
| --- | --- | --- |
| GET | `/api/v1/projects` | List Projects |
| POST | `/api/v1/projects` | Strict Project registration |
| GET | `/api/v1/projects/resolve?path=…` | Resolve without allocation |
| GET | `/api/v1/projects/ledger` | All-Projects ledger |
| GET | `/api/v1/projects/{projectId}` | Show Project |
| PATCH | `/api/v1/projects/{projectId}` | Update title/slug |
| GET | `/api/v1/projects/{projectId}/ledger` | Project ledger |
| POST | `/api/v1/projects/{projectId}/move` | Atomic explicit move |
| DELETE | `/api/v1/projects/{projectId}` | Tombstone empty Project |

#### Artifacts and Revisions

| Method | Path | Operation |
| --- | --- | --- |
| GET | `/api/v1/artifacts` | List Artifacts |
| POST | `/api/v1/artifacts` | Strict Publish |
| POST | `/api/v1/artifact-imports` | Ordered import batch |
| GET | `/api/v1/artifacts/{artifactId}` | Show Artifact |
| PATCH | `/api/v1/artifacts/{artifactId}` | Update title/description/slug |
| DELETE | `/api/v1/artifacts/{artifactId}` | Enter recovery |
| POST | `/api/v1/artifacts/{artifactId}/replace` | Publish/select Revision |
| POST | `/api/v1/artifacts/{artifactId}/restore` | Restore |
| POST | `/api/v1/artifacts/{artifactId}/pin` | Pin |
| POST | `/api/v1/artifacts/{artifactId}/unpin` | Unpin |
| POST | `/api/v1/artifacts/{artifactId}/purge-plans` | Plan early purge |
| POST | `/api/v1/artifact-purge-plans/{planId}/apply` | Apply early purge |
| GET | `/api/v1/artifacts/{artifactId}/revisions` | Revision history |
| GET | `/api/v1/revisions/{revisionId}` | Revision metadata |

#### Services and Targets

| Method | Path | Operation |
| --- | --- | --- |
| GET | `/api/v1/services` | List Services |
| POST | `/api/v1/services` | Strict registration |
| GET | `/api/v1/services/{serviceId}` | Show Service/Targets |
| PATCH | `/api/v1/services/{serviceId}` | Atomic presentation/Targets/action update |
| DELETE | `/api/v1/services/{serviceId}` | Catalogue-only removal |
| POST | `/api/v1/services/{serviceId}/teardown` | Explicit teardown |
| POST | `/api/v1/services/{serviceId}/refresh` | Refresh Service |
| POST | `/api/v1/services/refresh` | Refresh all |
| POST | `/api/v1/services/{serviceId}/pin` | Pin |
| POST | `/api/v1/services/{serviceId}/unpin` | Unpin |
| POST | `/api/v1/services/{serviceId}/keep` | Renew grace only |
| GET | `/api/v1/services/{serviceId}/targets` | List Targets |
| POST | `/api/v1/services/{serviceId}/targets` | Add Target |
| GET | `/api/v1/services/{serviceId}/targets/{targetName}` | Show Target |
| PATCH | `/api/v1/services/{serviceId}/targets/{targetName}` | Update URL/label |
| DELETE | `/api/v1/services/{serviceId}/targets/{targetName}` | Remove with primary replacement |
| POST | `/api/v1/services/{serviceId}/targets/{targetName}/promote` | Promote |
| POST | `/api/v1/services/{serviceId}/targets/{targetName}/refresh` | Refresh Target |

#### Cleanup, diagnostics, recovery, backup, configuration, audit

| Method | Path | Operation |
| --- | --- | --- |
| GET | `/api/v1/cleanup/preview?pressure=false` | Cleanup preview |
| POST | `/api/v1/cleanup/runs` | Run cleanup |
| GET | `/api/v1/cleanup/runs/{operationId}` | Cleanup state/result |
| GET | `/api/v1/system/health` | Readiness/health |
| GET | `/api/v1/system/status` | Fast status |
| POST | `/api/v1/system/diagnostics` | Normal/deep diagnostics |
| POST | `/api/v1/system/recovery/plans` | Create durable non-destructive plan |
| GET | `/api/v1/system/recovery/plans/{planId}` | Show plan |
| POST | `/api/v1/system/recovery/plans/{planId}/apply` | Apply plan |
| POST | `/api/v1/system/recovery/resume` | Resume intent |
| GET | `/api/v1/system/recovery/operations/{operationId}` | Show operation |
| POST | `/api/v1/system/backups` | Create backup |
| GET | `/api/v1/system/backups/{backupId}` | Backup state/result |
| POST | `/api/v1/system/backups/{backupId}/cancel` | Cancel nonterminal backup |
| POST | `/api/v1/system/backups/verify` | Verify backup |
| GET | `/api/v1/system/configuration` | Redacted effective configuration |
| POST | `/api/v1/system/configuration/validate` | Validate proposed TOML without activation |
| GET | `/api/v1/system/audit` | Paginated audit |

There is no remote endpoint for release installation/activation, setup check/apply/remove/uninstall, systemd service status/start/stop/restart, stable command links, configuration installation, or Tailscale Serve mutation. In particular, `/api/v1/system/setup`, `/api/v1/system/service`, and aliases are absent.

### Batch and bulk results

Trustworthy import, refresh, cleanup, diagnostic, backup-verification, and recovery fan-out outcomes always use `ok:true` on stdout. Per-entry failures live in `result.items[].error`. Aggregate fields are exact:

| `overall` | Meaning | `partial` | CLI exit |
| --- | --- | ---: | ---: |
| `complete` | every entry succeeded or replayed unchanged | false | 0 |
| `partial` | at least one succeeded and at least one failed/skipped | true | 8 |
| `failed` | zero succeeded and at least one trustworthy entry failure exists | false | 8 |

Empty input or inability to establish trustworthy entry outcomes is `ok:false` on stderr with the command-level exit. Diagnostics retain the broader rule that omitted/errored/skipped requested checks set `partial:true`; completed unhealthy diagnostics exit `10`.

### HTTP status and CLI exit mapping

| HTTP | Stable meaning | CLI exit |
| ---: | --- | ---: |
| 200 | read/mutation/replay/trustworthy fan-out | 0, 8, 9, or 10 by result |
| 201 | strict resource creation | 0 |
| 202 | durable nonterminal operation | 0 |
| 400 | malformed JSON/query encoding | 2 |
| 404 | `not_found` | 3 |
| 409 | strict/idempotency/cursor/domain conflict | 4 |
| 410 | `gone` | 3 |
| 412 | `changed_record` | 4 |
| 413 | metadata body too large | 2 |
| 415 | unsupported media | 2 |
| 422 | validation/unsafe input; `source_changed` exception | 2 or 7 |
| 423 | maintenance/authority lock | 6 |
| 428 | `precondition_required` | 4 |
| 429 | bounded queue exhausted | 6 |
| 500 | internal | 10 |
| 503 | daemon/storage authority unavailable | 5 or 10 |
| 507 | capacity/reserve/ceiling | 10 |

`204` and WebDAV `207` are not used; envelopes are always returned.

## 8. Agent CLI contract

The grammar is resource namespaces:

```text
obs [GLOBAL OPTIONS] <RESOURCE> <ACTION>
```

`obs --help` lists all namespaces; `obs <resource> --help` lists that resource's complete leaves, runnable synopses, and examples. Parent help exits `0`. Global options are `--server URL`, `-p|--project PATH`, `--json`, `--timeout DURATION`, `--idempotency-key KEY`, and leaf-specific `--yes`. `--yes` is accepted only where confirmation is required.

### Artifact leaves

```text
obs artifact publish SOURCE
  [--entry PATH] [--title TEXT] [--description TEXT] [--slug TEXT]
  [--ttl DURATION | --pin] [--reason TEXT]
obs artifact replace ARTIFACT SOURCE
  [--entry PATH] [--title TEXT] [--description TEXT] [--slug TEXT]
  [--ttl DURATION | --pin] [--reason TEXT] [--record-version VERSION]
obs artifact import SOURCE...
  [--entry PATH] [--title TEXT] [--description TEXT] [--slug TEXT]
  [--ttl DURATION | --pin] [--reason TEXT] [--options FILE]
obs artifact list
  [--all] [--state STATE] [--retention MODE] [--query TEXT]
  [--order recent|title|attention] [--limit N] [--after CURSOR]
obs artifact show ARTIFACT [--revisions]
obs artifact remove ARTIFACT --yes [--record-version VERSION]
obs artifact restore ARTIFACT [--ttl DURATION | --pin] [--reason TEXT]
obs artifact pin ARTIFACT [--reason TEXT]
obs artifact unpin ARTIFACT [--ttl DURATION]
obs artifact purge preview ARTIFACT
obs artifact purge apply PLAN --yes
```

`--options FILE` is a JSON array with one nullable object per positional source and only the per-entry overrides in [section 5](#explicit-import-and-per-entry-outcomes).

### Service and Target leaves

```text
obs service register NAME
  --target NAME=URL...
  [--primary NAME] [--label TEXT] [--description TEXT] [--slug TEXT]
  [--teardown-arg ARG...] [--action-timeout DURATION]
  [--pin] [--reason TEXT]
obs service update SERVICE
  [--label TEXT] [--description TEXT] [--slug TEXT]
  [--teardown-clear | --teardown-arg ARG...]
  [--action-timeout DURATION] [--record-version VERSION]
obs service list
  [--reachability STATE] [--pinned BOOL] [--query TEXT]
  [--order recent|title|attention] [--limit N] [--after CURSOR]
obs service show SERVICE
obs service remove SERVICE --yes [--record-version VERSION]
obs service teardown SERVICE --yes
  [--action-timeout DURATION] [--record-version VERSION]
obs service refresh SERVICE
obs service refresh --all
obs service pin SERVICE [--reason TEXT]
obs service unpin SERVICE
obs service keep SERVICE
obs service target list SERVICE
obs service target show SERVICE TARGET
obs service target add SERVICE NAME=URL [--label TEXT] [--primary]
obs service target update SERVICE TARGET [--url URL] [--label TEXT]
obs service target remove SERVICE TARGET --yes [--new-primary TARGET]
obs service target promote SERVICE TARGET
obs service target refresh SERVICE TARGET
```

### Project, cleanup, system, and serve leaves

```text
obs project list
  [--all] [--query TEXT] [--order title|recent]
  [--limit N] [--after CURSOR]
obs project show PROJECT
obs project resolve [PATH]
obs project register [PATH] [--title TEXT] [--slug TEXT]
obs project update PROJECT [--title TEXT] [--slug TEXT]
obs project move PROJECT NEW_PATH --yes [--title TEXT] [--slug TEXT]
obs project tombstone PROJECT --yes

obs cleanup preview [--pressure]
obs cleanup run --yes [--pressure]

obs system status
obs system diagnostics [--deep]
obs system recovery preview OPERATION [SELECTOR...]
obs system recovery apply PLAN --yes
obs system recovery resume [OPERATION]
obs system backup create DESTINATION
obs system backup verify BACKUP [--deep]
obs system backup cancel BACKUP --yes
obs system config show
obs system config validate FILE
obs system setup check
obs system setup apply --yes
obs system setup remove --yes
obs system setup uninstall --yes
obs system service status
obs system service start
obs system service stop --yes
obs system service restart --yes

obs serve
  [--listen 127.0.0.1:3773]
  [--canonical-origin URL]
  [--storage PATH]
  [--max-stored-bytes BYTES]
  [--max-live-artifacts COUNT]
  [--teardown-timeout DURATION]
```

Recovery operations are exactly `reconcile`, `quarantine`, `repair_catalogue_candidate`, `salvage_catalogue_candidate`, `rebuild_catalogue_candidate`, `validate_candidate`, `activate_candidate`, `restore_backup`, and `discard`.

### Output, confirmation, and daemon boundary

Human results use stdout; warnings/progress/log guidance use stderr. JSON success/trustworthy outcomes emit one stdout value. Command inability emits one stderr error with empty stdout. Existing-resource CLI mutations translate record version into API `If-Match`; selectors follow Project ID/key/path, Artifact ID/key, Revision ID, Service ID/key or Project-scoped name, and exact Target name rules. Ambiguous Service name is rejected.

TTY destructive calls may prompt once. Non-TTY calls never prompt and require `--yes`. Idempotency follows [section 7](#common-api-contract): non-TTY/automation mutation requires a supplied key; interactive TTY may generate/display one. Future agents MUST always supply one.

`system config show` returns the daemon's redacted active effective configuration. `system config validate FILE` requires FILE: the CLI opens that local regular file through the safe no-follow input boundary, reads its TOML contents, and sends content—not a path—to `POST /api/v1/system/configuration/validate`. There is no stdin mode. The daemon returns ordered parse, schema, and semantic checks and never installs or activates the proposal. This POST is non-mutating and needs neither `If-Match` nor `Idempotency-Key`; daemon absence exits `5`.

All commands except the exact eight local setup/service leaves call the daemon. The CLI never opens SQLite/storage or autostarts the daemon. Missing daemon is `daemon_unavailable`, exit `5`. `--server` selects daemon-backed commands and is a usage error on local setup/service leaves. Setup check may reuse the same pure TOML parser/schema module locally, but that module has no domain/SQLite/storage dependency and does not add a local-authority leaf.

## 9. Project-led browser interface

### Ledger and navigation

Observatory opens as a Project-led catalogue, not an activity feed or search-first surface. All Projects is the default first level; each Project shows its canonical directory context. The second level is one ledger of Artifacts and Services. Default order is most recent observation/Publish; Title and Needs attention are alternatives.

Each row shows Entry kind/identity cue, title, description, lifecycle or reachability, size/Revision or diagnostics, recency, Project, and separate Open/Details actions. Artifact states are current, pinned, expiring, and recoverable with retention/deadline/current Revision/logical size. Service states are online, offline, unknown, and stale with host-vantage age/status/duration/failure and expiry warning. Reachability is not application health.

Artifact Open uses stable current bytes. Service Open leaves Observatory for the direct primary Target. Details stays under Observatory. Alternatives are explicit named choices only in Service details. Secondary search operates inside Project scope over visible fields and never replaces Project navigation or hides lifecycle. Kind filters are All/Artifacts/Services. The ledger never executes or embeds Entry previews.

Mobile uses horizontally scrollable Project navigation and stacked cards preserving the same state, diagnostics, recency, Open, and Details. Baseline accessibility includes semantic landmarks/headings, skip link, labels, keyboard navigation, visible focus, non-stealing status announcements, sufficient contrast, 200% reflow, reduced motion, and core server-rendered navigation without JavaScript.

### Browser mutation inventory

The UI exposes only:

- Project: register by typed daemon-host path, update title/slug, explicit move, tombstone empty Project;
- Artifact: update title/description/slug, pin/unpin, remove, restore, early-purge preview/apply;
- Service: update presentation/action, pin/unpin, keep, refresh, remove, teardown, and Target add/update/remove/promote/refresh;
- Operations: cleanup preview/run, normal status/diagnostics, and read-only recovery/backup/audit state.

The UI excludes Publish, replace, import, deep diagnostics, recovery apply/resume, backup create/restore, candidate activation/discard, configuration installation, every setup/service-manager leaf, and arbitrary host-path selection.

No GET mutates. Confirmation is ordinary submit for metadata/pin/keep/refresh/non-primary Target changes; a resource/version/effect review for Artifact remove/restore, Service remove, and primary Target removal; exact Service name plus process consequence for teardown; exact Project key plus Service/Artifact consequences for move/tombstone; exact cleanup preview for run; and exact purge plan plus Artifact key/permanent warning for early purge.

Every browser mutation MUST pass exact canonical Host/authority; exact Origin, or exact-origin Referer only when Origin is unavailable; `Sec-Fetch-Site: same-origin` when supplied; one-use CSRF token bound to action/resource/version/confirmation; `If-Match`; and idempotency key. The cryptographically random token expires in ten minutes, is consumed only on accepted dispatch, is not a login/session, and cannot authorize another action/version. JavaScript sends `X-Observatory-CSRF`; non-JavaScript form adapters perform identical validation/application operation then `303` to a server-returned detail URL. Cross-origin/missing-source-origin/mismatched Host returns `403 browser_origin_rejected`. Loopback non-browser clients do not need CSRF but still need idempotency/preconditions.

## 10. Artifact retention and capacity

Every Artifact has exactly one retention mode:

| Mode | Behavior |
| --- | --- |
| Default | Expire 30 days after latest successful Publish/replacement; views do not renew. |
| Explicit TTL | Positive requested duration from successful Publish; replacement restarts unless changed. |
| Pinned | No deadline until unpin/delete; unpin selects TTL or starts default at unpin. |

Retention belongs to Artifact identity; failed Publish changes nothing. Times are absolute UTC. Pin reason is visible optional metadata. Pin protects Artifact/current Revision, not unlimited history.

At deadline, Artifact is expired before cleanup, leaves discovery, and stable URL returns `410`. Expiry and normal deletion start a seven-day recovery window. Restore chooses a mode. After it, cleanup may purge bytes/Revisions while retaining tombstone identity, Project, title, time, cause, and reclaimed bytes. Early purge requires exact unexpired plan, matching record, explicit confirmation, no active lease/current reference, durable tombstones, and audit.

A superseded Revision is normally eligible only when older than seven days **and** outside the five newest superseded Revisions. Pressure may remove superseded Revisions inside those windows oldest-first but preserves every live current Revision. Removed Revision URL is `410` and ID is not reused.

There are no default byte/file/live-count ceilings. Configured `max_stored_bytes` counts logical served-file bytes of every owned Revision not physically discarded: current, superseded, recoverable, unavailable, quarantine pending deletion, plus source-copy staging reservations. It excludes `.obs.json`, recovery manifests, SQLite/WAL/audit metadata, exported backups/recovery candidates, and external Service data. Each byte-adding operation reserves exact source regular-file lengths; replacement reserves new bytes without subtracting current.

`max_live_artifacts` counts discoverable, non-expired stable Artifact identities in live state. Revisions, tombstones, expired, deleted-recoverable, and gone identities do not count. Replacement does not increase it; restore does.

Filesystem safety separately reserves `max(1 GiB, ceil(filesystem_total_bytes × 0.05))`. Before Publish, cleanup order is abandoned staging; normally eligible superseded Revisions; expired/deleted past recovery; then additional superseded oldest-first. Cleanup never shortens recovery or evicts a live current Revision.

Capacity results contain `requiredBytes`, `accountedStoredBytes`, `maxStoredBytes`, `liveArtifacts`, `maxLiveArtifacts`, `filesystemAvailableBytes`, `reserveBytes`, `reclaimableBytes`, and `blockingConstraint`. Failure happens before commit; existing reads continue.

Preview lists candidates, reasons, Revisions, bytes, and recoverability. Every expiry, restore, retention mutation, Revision removal, deletion, purge, pressure cleanup, and failure appends timestamp/actor/cause/IDs/bytes. Cleanup is single-writer, restart-safe, idempotent intent/quarantine/finalize/delete. Independent candidates continue after one failure. Persistent failure/reserve breach blocks byte additions while intact reads continue.

## 11. Service reachability and expiry

The backend probes every Target from its host/network namespace using direct HTTP(S) GET, no ambient proxy, no redirects, and five-second header deadline. Any HTTP response is reachable; DNS/connection/TLS/protocol/timeout is unreachable. Status is diagnostic, not application health.

Targets schedule every 60 seconds with up to 10% jitter, once after startup, and immediately after committed relevant mutation. Writes do not wait; page views do not probe. Current-version state is unknown with no result; online/offline for response/failure no older than two minutes; stale thereafter with underlying fact labeled. Scheduler/internal failure creates no offline observation. Late old-version results are discarded. URL/add/rename/delete invalidation is exact; unchanged URL preserves observation.

Services are unpinned by default. Seven-day grace begins only when every current Target is current-version offline and none online. Online resets; unknown/stale neither starts nor completes. Target mutation, keep, or unpin starts fresh grace when all-offline. Pin suppresses expiry, not probes. Reachable Services have no age-only lease.

Cleanup runs at least hourly and freshly probes every Target at deadline under normal bounds. It deletes only on all-offline completed results and unchanged record version. Reachability, internal failure, unknown, concurrency, or incomplete confirmation preserves. Automatic deletion never invokes teardown.

Manual refresh supports Target, Service, or all. Concurrency is 16 global, two per destination host, one per Target; duplicates coalesce; every probe source shares bounds. Ordered trustworthy partial results follow [section 7](#batch-and-bulk-results). Details show URL/state/time/duration/status or stable failure category, host vantage, pin/activity/expiry without credential leakage.

## 12. Persistence, backup, and crash protocols

### SQLite authority and private layout

SQLite is authoritative. The private data root is:

```text
catalogue.sqlite
staging/<operation-id>/
revisions/<revision-id>/
quarantine/<operation-or-revision-id>/
backups/<backup-id>/
candidates/<candidate-id>/
```

A final Revision has exact served tree plus reserved non-served recovery manifest with schema, IDs, entry, counts, Publish instant, ordered path/size/digest; SQLite stores manifest digest. Paths derive only from opaque IDs.

Catalogue/sidecars/staging/Revisions/quarantine/source backup workspace/candidates share one supported local filesystem wherever private atomic rename applies. Use STRICT tables, constraints, `foreign_keys=ON` every connection, local WAL, `synchronous=FULL`, bounded busy handling, short `BEGIN IMMEDIATE`, decided indexes, compare-and-swap versions, and latest Target observation only.

Publish protocol is durable intent; descriptor-relative stage/validate/checksum/manifest/sync; atomic final Revision rename plus parent sync; then short SQLite visibility/current/retention/audit commit. Startup completes/resumes matching intents only and quarantines malformed/mismatched/unreferenced bytes.

Cleanup transactionally rechecks/marks unavailable/audits; atomically renames complete Revision to quarantine and syncs parents; finalizes tombstone/bytes; unlinks asynchronously. Missing bytes are error, not proof.

### External backup topology

`backups/<backup-id>/` is a source-side SQLite snapshot, workspace, and non-authoritative receipt directory. It contains the Online Backup snapshot and operation/receipt evidence, not duplicate Revision payload. SQLite operation/backup/lease rows remain authority. Revision bytes copy directly from leased immutable Revision directories. The workspace may be removed after completion/lease release; path presence cannot create, complete, cancel, renew, or release anything.

`obs system backup create DESTINATION` names one exact absolute host-local final directory. Parent exists/accessibly; leaf is absent; no ancestors are created; relative/root/empty/dot/dotdot/symlinked/ambiguous paths fail. Backup gets never-reused 26-character ID. Destination sibling staging is `.<leaf>.observatory-<backup-id>.incomplete`, mode `0700`, no-follow/no-replace. Unknown collision is untouched. Finalization never overwrites, merges, deduplicates, or forces.

Write-capable destination filesystem is only Linux btrfs, ext4, or XFS **and** MUST pass an active disposable probe proving exclusive/no-follow file create, write/read, file+directory sync, `RENAME_NOREPLACE` success/collision, child-directory rename, cleanup, stable mount/parent identity. Network/FUSE/9p/virtiofs/overlay/tmpfs/read-only/unknown fails. `statfs`, mountinfo, and `statx` mount identity are rechecked through finalization.

Destination bundle is:

```text
catalogue.sqlite
revisions/<revision-id>/<served tree and recovery manifest>
backup-manifest.json
.observatory-complete
```

The manifest binds format/backup/build/API/schema, instants, catalogue length/digest, exact ordered leased Revision set and every member path/size/digest, totals, verification. It excludes private paths, mount/inode/owner/mode, teardown argv, and secret URLs. Format v1 MUST freeze one canonical serialization/digest fixture before emission. Completion marker is written last and binds backup ID plus manifest/catalogue digests; it is evidence only. A backup is valid only at final name with exact marker/manifest/catalogue/Revision inventory.

Creation order is authoritative intent; Online Backup snapshot; exact Revision-set lease under short availability barrier; source/destination capacity; destination classification/probe; hidden stage; copy/hash/sync/reopen catalogue; copy/hash/sync exact Revisions; bottom-up directory sync; write/sync manifest; deep reread verification; write/sync marker; recheck parent/final absence; `renameat2(..., RENAME_NOREPLACE)`; sync destination parent; mark completed/audit/release lease in SQLite; asynchronously clean source evidence. Cross-filesystem transport is verified copy, never rename. Success returns only after SQLite completion.

Every member is newly created `0600`/directory `0700`; links/devices/FIFOs/mounts/extras fail. Ownership, ACL/xattrs, links/reflinks, sparseness, and source modes are not preserved. Checked copy loops require exact length/digest.

Source capacity includes snapshot/workspace/receipt and preserves normal reserve; Revision payload is not duplicated there. Destination requires catalogue + exact Revision/manifest/marker + allocation overhead minus deeply verified reusable staging and preserves its own greater-of-1-GiB-or-5% reserve. Capacity error returns filesystem class, required/available/reserve/already-staged bytes without path.

### Backup crash, resume, cancel, and privacy

Crashes before intent leave no operation; thereafter SQLite phase drives resume. Incomplete source snapshot may be recreated; exact lease protects Revisions; destination partial work stays hidden; allegedly completed files are rehashed; marker-before-rename remains invalid by location; rename leaves hidden or complete final, never half; after parent sync before SQLite completion exact evidence permits completion/lease release. Absence never proves success.

Client disconnect/timeout does not cancel. SIGTERM stops safely and leaves resumable state. `backup cancel BACKUP --yes` and `POST /api/v1/system/backups/{backupId}/cancel` require current If-Match/idempotency, record `cancel_requested`, quiesce current file, record `cancelled`, release lease, retain exact incomplete evidence. Completed is `409 backup_completed`. Durable finalization winning a race yields completed.

Resumable state and lease persist 24 hours after last durable progress. Then reconciler may mark abandoned/release only after no worker/finalization. Exact matching staging may be deleted; ambiguity becomes `stale_ambiguous` and requires exact discard. Final external backup is operator-owned and never retention-cleaned.

Stable backup states are `intent_recorded`, `snapshotting`, `leasing`, `probing_destination`, `copying`, `verifying`, `completion_marker_synced`, `finalizing`, `completed`, `cancel_requested`, `cancelled`, `failed_resumable`, `failed_terminal`, `abandoned`, `stale_ambiguous`.

Remote API never exposes absolute destination/storage/Project/home/mount paths. It returns safe basename/redacted. Local CLI may echo only its caller-supplied DESTINATION. Manifests/audit/logs/errors exclude those paths, source identities, owner/inode, contents, argv, secret URLs. Verification accepts exact finalized path or known backup ID, never `.incomplete`. Normal verifies path/marker/binding/schema/quick/foreign keys/inventory/manifest/types/sizes/no extras; deep adds integrity check and complete rehash. Database alone is `catalogue_only`.

Restore deep-verifies supported local finalized input, rejects remote/FUSE/incomplete/symlink/overlap/change, records exact loss/conflict/capacity effects, copies into private `staging/<operation-id>`, re-verifies, syncs, atomically renames to candidate, validates, and never serves/links/depends on external bytes. Activation remains separate and authority-last.

## 13. Storage diagnostics and recovery

### Profiles, checks, and health gates

| Profile | Contract |
| --- | --- |
| `system status` | Fast non-mutating check of filesystem/capacity, SQLite open/application/schema, WAL, intents/staging/quarantine/leases/cleanup counts, and live-current path existence. |
| `system diagnostics` | Online bounded snapshot adding quick/foreign-key, passive checkpoint, all paths, manifest parse/version/digest, intents, staging/quarantine, leases, cleanup. No full content hash. |
| `system diagnostics --deep` | Global maintenance gate adding integrity check, every path/size/digest, owned orphan scan, named backup verification, and disposable private-filesystem capability probe. Catalogue reads/mutations/probes/Artifact serving fail with maintenance response; Services remain untouched. |

`ok` is execution; `result.health` is `healthy|degraded|unhealthy|offline`. Ordered checks contain id, `pass|warn|fail|error|skipped`, stable state/category/message/retryable/scope/time/duration/redacted details. Omitted/error/skipped requested work sets partial. Complete healthy/degraded exits 0; trustworthy partial 8; completed unhealthy 10.

Required checks/states:

| Check | Stable states |
| --- | --- |
| `sqlite.open` | `open|not_found|permission_denied|busy|io_error|not_database` |
| `sqlite.application` | `matches|mismatch|unreadable` |
| `sqlite.schema` | `supported|older_migration_required|newer_unsupported|invalid` |
| `sqlite.quick|integrity|foreign_keys` | `ok|violations|not_run` |
| `sqlite.wal` | `clean|frames_pending|checkpoint_blocked|wal_io_error|not_wal` |
| `storage.intents` | `terminal|interrupted_resumable|interrupted_ambiguous|invalid_transition` |
| `revision.path` | `present|missing|wrong_type|unexpected` |
| `revision.manifest` | `valid|missing|parse_error|unsupported_version|catalogue_digest_mismatch` |
| `revision.content` | `valid|missing_bytes|size_mismatch|digest_mismatch|unsafe_member|not_run` |
| `storage.staging` | `clear|active|abandoned|unowned_or_ambiguous` |
| `storage.quarantine` | `clear|retained|orphaned|purge_failed` |
| `storage.backup_leases` | `active|expired_releasable|stale_ambiguous|invalid_scope` |
| `storage.cleanup` | `ok|interrupted|candidate_failed|persistent_failure` |
| `storage.filesystem` | `supported|read_only|cross_mount_layout|remote_or_unsupported|capability_failed` |
| `storage.capacity` | `within_reserve|reserve_at_risk|reserve_breached|capacity_unknown` |

Categories are `catalogue|schema|integrity|wal|content|operation_interrupted|missing_bytes|quarantine|lease|cleanup|filesystem|capacity|contention|permission|internal`. Service reachability is separate.

Wrong/new/invalid/corrupt catalogue or unsupported private storage exposes no Entries and blocks writes/probes. Missing/corrupt Revision disables affected Revision and blocks byte additions. Interrupted Publish/cleanup blocks byte additions until classified. Reserve/cleanup failure blocks byte additions but permits healthy metadata/probes/intact reads/recovery. Checkpoint contention is degraded; WAL I/O blocks writes. Stale lease blocks cleanup only for named Revisions.

### Plan/apply and operation-specific recovery

Recovery preview is **non-destructive to catalogue authority and Artifact bytes**, but durably records plan and audit metadata. It returns plan ID, operation, exact selectors/identity/digest fingerprints, health generation, estimated bytes, availability effect, ambiguity/loss details, preconditions, rollback point, expiry, and confirmation. Apply accepts only the exact unexpired plan, rechecks fingerprints/preconditions before irreversible phases, never broadens, and records intent before effects. Resume continues only the same nonterminal intent. Locks are global for authority changes and per-Revision where sufficient. Crashes leave old or new complete authority.

Operation semantics are exact:

- `reconcile`: sole automatic startup operation; completes only matching recorded Publish/cleanup/lease phases, resumes intact staging, quarantines malformed/mismatched/unreferenced owned bytes, never adopts them.
- `quarantine`: marks committed Revision unavailable before same-filesystem move, preserves bytes/evidence, and requires preview/confirmation for available content.
- `repair_catalogue_candidate`: requires readable catalogue with known Observatory application and schema identity; snapshots source, reconstructs only application-derivable structures/state under known schema, preserves source, and does not claim unknown-row repair.
- `salvage_catalogue_candidate`: runs SQLite recovery into separate evidence, then accepts only records satisfying current schema, identity, constraints, and cross-resource validation; returns every accepted, rejected, lost, ambiguous, and synthesized record. Recovery rows never directly become authority.
- `rebuild_catalogue_candidate`: starts empty current schema and uses valid manifests/content only for byte inventory. It identifies unrecoverable current selection, retention/pins, tombstones, audit, Services, Targets, Teardown Actions, and observations; no directory becomes visible automatically.
- `validate_candidate`: runs application/schema, quick/integrity/foreign-key, intent, full manifest/content, uniqueness, tombstone, and referential checks plus exact loss/ambiguity details. Failures cannot be waived; resolutions create a new candidate/plan.
- `activate_candidate`: requires fully passing candidate, offline maintenance, fresh exact plan, confirmation; durably stages, atomically selects catalogue/WAL unit, retains former catalogue/evidence, records activation. Rollback is a new validated activation plan.
- `backup verify`: normal/deep behavior is defined in [section 12](#backup-crash-resume-cancel-and-privacy), returns exact missing/extra/mismatch outcomes, and never calls database-only input complete.
- `restore_backup`: preview deep-verifies into isolated context and returns Entries/bytes/audit newer than backup that would be lost, opaque-ID conflicts, required source/candidate/rollback capacity, and exact cutover/rollback paths in redacted form. Apply uses offline gate, bytes-first/authority-last cutover, retains old authority/displaced bytes.
- `discard`: sole irreversible storage operation. Exact preview lists IDs/digests/bytes/consequences/no-reference proof. Apply requires confirmation, unexpired plan, no lease/reference, durable intent. It cannot remove only/active catalogue, active candidate, live current Revision, or nonterminal evidence; early Artifact purge also enforces recovery contract.

Every diagnostic gate change, plan, operation phase, reconciliation, quarantine, backup lease, candidate, activation/rollback, cleanup/discard, actor, IDs, digests, bytes, category, and durability phase is append-only audited. Private paths, argv, secret URLs, SQL/page/file/recovery-row content are redacted.

## 14. Network and security boundary

Observatory is private/tailnet-only. Canonical HTTPS origin defaults to `https://desktop.greyhound-chinstrap.ts.net/`; 443 is recommended/omitted. Another MagicDNS host/port is explicit migration and old-origin redirects are not required. Backend binds loopback only; local port is independently configurable and absent from public links. Exact origin MUST match active Serve host/port or setup blocks/diagnoses without guessing.

Deployment owns only canonical host/port root Serve handler to loopback. Setup inspects, refuses conflicts/matching-unowned adoption, preserves unrelated handlers, and verifies exact post-state. Ordinary daemon startup is Serve-read-only. Remove only receipt/live-tuple proven ownership. Observatory never owns Service handlers.

Tailscale is sole remote authentication boundary, but membership alone is insufficient. Explicit least-privilege grant permits intended humans/agent devices to node HTTPS port; Will is default human principal and tagged/headless agents need explicit grant. Authorized principals receive full capabilities. There are no Observatory accounts/login/cookies/passwords/API keys/roles.

Loopback prevents LAN/tailnet bypass and forged Serve headers. Identity headers may attribute but are not second authorization. Browser mutations enforce [section 9](#browser-mutation-inventory). Funnel/public and LAN-only/non-tailnet clients are prohibited. Certificate Transparency name disclosure is accepted.

Service Targets are independent origins; operator owns grants/TLS/auth/ports/proxy correctness. Root-oriented Service uses its own root Serve mount, not Observatory path. Canonical Service release validation proves actual loopback bind, root routes, public HTTPS origin/forwarded handling, live transport from host+remote client, and persistent Serve restart/rollback. Unmodified Sideshow 0.7.0 fails bind/origin and is not canonical.

## 15. Implementation, bootstrap, and supervision

### Chosen stack and runtime

One Rust Cargo binary `obs` uses Tokio+axum, clap, serde, rusqlite with pinned bundled upstream SQLite/backup, Observatory-owned rustix Linux filesystem deep module, and reqwest default-disabled with rustls/web-PKI roots. It embeds Project-led HTML/CSS/ES modules; no Node/Bun/npm/SPA/client router/runtime. Static build-ID assets get one-year immutable cache/strong ETag; UI/API are `no-store`.

Tokio starts with four worker and at most four blocking threads plus bounded queues. Load evidence MAY reduce either fixed limit; it MUST NOT use host-CPU-derived defaults or unbounded blocking. Probe bounds remain independent. Daemon takes `$XDG_RUNTIME_DIR/observatory/daemon.lock`; second exits before domain effects. Normal commands never autostart.

Release target is pinned/locked `x86_64-unknown-linux-gnu`; desktop baseline is kernel 7.0.14, glibc 2.43, Rust 1.96.1, systemd 261, Tailscale 1.98.8, local btrfs. Build glibc baseline is no newer than 2.43. Bundle includes stripped binary, symbols, SHA-256 manifest, detached signature, signed provenance binding source/lock/toolchain/target/command, SBOM, licenses/notices, and passing format/Clippy/tests/dependency/vulnerability/cargo-deny.

### Local bootstrap authority

Exactly these eight leaves may run without daemon:

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

They use common output envelope with `authority:"local_setup"`; `--server` is usage error. Capability allowlist permits release verification/compatibility; private install/config/unit/lock/receipt files; fixed `systemctl --user`; fixed noninteractive handler-specific Tailscale status/mutation; bounded loopback `/api/v1/system/health`; canonical HTTPS check; redacted process/socket/journal metadata.

Adapter MUST NOT import/call SQLite factory/catalogue repository, Revision/staging/quarantine/backup/candidate/recovery modules, domain dispatcher/mutation services, or authority discovery helpers. It MUST NOT open/inspect catalogue, WAL/SHM, Revisions, backups, candidates, staging, or quarantine. Daemon alone creates/opens data root, migrates/reconciles/diagnoses/recovers/backs up/cleans/serves.

### External release trust and activation

First install begins outside bundle: operator verifies complete bundle SHA-256, signature, provenance against published identity/pinned key before running it. Candidate repeats consistency/digest/signature/provenance-subject/target/version checks as defense in depth. Bare/incomplete bundle, symlink/non-regular/unexpected hard link, mismatch, unsupported target/provenance fails.

Exact first journey is download/unpack; externally verify; `./obs system setup check --json`; resolve preconditions; `./obs system setup apply --yes --idempotency-key <unique-key>`; then use `~/.local/bin/obs`. No download/package-manager/sudo/system service.

Install layout is:

```text
$HOME/.local/lib/observatory/versions/.staging-<version>-<operation-id>/{obs,release.json}
$HOME/.local/lib/observatory/versions/<version>/{obs,release.json}
$HOME/.local/lib/observatory/current -> versions/<version>
$HOME/.local/lib/observatory/install-state.json
$HOME/.local/bin/obs -> ../lib/observatory/current/obs
```

Activation validates user/HOME/ownership/permissions/target/glibc; copies verified members to unique staging; rehashes/rechecks provenance; sets private permissions; syncs files/directory; atomically renames to version and syncs versions; atomically replaces temporary relative `current` symlink and syncs parent; atomically creates relative stable command; resolves links/verifies digest. Same verified version may reuse; same version/different bytes is `ownership_conflict`. Prior version survives health and at least one later activation; never remove only rollback candidate.

### XDG creation, setup lock, and receipts

`setup check` creates nothing. `setup apply` may create config directory/file, runtime coordination, install state, selector, receipt, unit; daemon startup alone creates data root. XDG paths are absolute. Runtime is user-owned local `0700`; invalid/missing is hard precondition.

Every mutating local leaf creates runtime subdirectory `0700`, acquires exclusive `setup.lock` within five seconds, or returns retryable contention without mutation. Check/status are lock-free snapshots and show `snapshotStable:false` during setup. Setup never takes daemon lock.

Non-TTY local mutations require supplied idempotency; TTY may generate/display. `install-state.json` records non-secret install/operation/key/fingerprint, current/prior releases/digests, unit/config digest/schema, loopback/origin, owned Serve tuple/fingerprint, phase/result/partial/rollback. Temporary write + file sync + atomic rename + parent sync. Receipt proves deployment ownership only.

### Exact setup check and apply

`setup check` returns ordered checks/actions for:

1. complete bundle digest/signature/provenance/self-check/target;
2. non-root matching UID/home/no sudo-derived target;
3. absolute XDG/protected runtime;
4. destination ownership/permissions/filesystem/capacity;
5. versions/current/stable command/receipt;
6. candidate/prior schema compatibility metadata;
7. config parsing/precedence/loopback/storage/config-migration preview;
8. prospective storage parent support without opening/creating storage;
9. setup-lock occupancy;
10. systemd user manager;
11. linger;
12. exact unit digest/drift/enable/active;
13. MainPID/cgroup/executable/last failure;
14. loopback occupancy/daemon health identity;
15. Tailscale CLI/daemon/login/node DNS/HTTPS/Funnel exclusion;
16. Serve JSON classification;
17. root absent/owned/matching-unowned/conflict;
18. canonical-origin host/port agreement; and
19. external tailnet-grant verification requirement.

It creates nothing and does no writable probe. Missing linger blocks with argv-safe operator command but never invokes sudo/loginctl. Unrelated failed units warn only.

`setup apply --yes` order is setup lock/replay; full recheck; refuse foreign/ambiguous conflicts before writes; durable candidate stage; record rollback state; create defaults or exact atomic config migration; install exact unit/reload; explicitly enable; atomically activate candidate; start/restart only owned unit; bounded exact loopback build/API/reconciliation/migration/listener/health gate; re-read and add only absent/owned Serve root; verify Serve JSON and canonical HTTPS; sync terminal receipt.

Only `/api/v1/system/health` is queried locally. Permitted storage/Tailscale-only degraded may warn; unhealthy/offline fails deployment objective but preserves diagnostic daemon. Local health commits installation; later Tailscale failure yields trustworthy partial exit 8 with Serve pending, not rollback. Preexisting root conflict blocks before local mutation; late conflict preserves healthy local install and root.

### systemd and Serve ownership

Generated unit is exact:

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

It omits incompatible home/tmp/syscall/dynamic-user/strict-system restrictions and logs stderr to journald. Apply reloads/enables/starts; service start requires installed owned unit and does not enable/install; stop verifies inactivity and leaves Serve/data; restart verifies exact selected build/API; status is inert. Clean stop does not restart; failure waits five seconds, max five starts/60 seconds. SIGTERM stops dispatch, finishes short transactions, cancels probes, leaves resumable long intents, bounded checkpoint, closes within 30 seconds.

Serve ownership tuple is canonical MagicDNS host + HTTPS port + root `/` + loopback target. States are `absent|owned_exact|matching_unowned|conflicting|unknown`; only absent/owned exact mutate. Never adopt matching unowned, reset whole config, touch Funnel/grants/other handlers. Apply mutates last and verifies unrelated state. Remove requires receipt+live tuple.

### Upgrade, rollback, remove, uninstall

Only daemon migrates catalogue after complete verified backup. Bootstrap observes health only. Config migration previews exact fields/restart, retains versioned backup, temporary validate/atomic replace/sync, fails unknown fields, retains config on remove/uninstall.

Executable rollback is automatic only before migration commit or when prior binary supports resulting schema. Uncertain/incompatible post-migration failure keeps candidate selected/prior executable+backup, sets `automaticRollbackAllowed:false`, and directs daemon diagnostics/recovery. Bootstrap never restores data.

`setup remove` locks/rechecks, refuses drifted root, removes exact owned root, stops/disables unit, removes matching unit/reloads/verifies, and retains versions/current/stable command/config/all data plus tombstone receipt. `setup uninstall` first removes, then deletes only verified stable symlink/current/version/receipts while retaining config and full data root. Neither opens/purges data; foreign nominal path is conflict. Repeated remove is verified unchanged success.

## 16. Configuration, defaults, errors, and health

Configuration precedence is CLI, environment, `config.toml`, built-in default per field. Secret-free; no live reload; restart required; SIGHUP states unsupported.

```toml
[server]
listen = "127.0.0.1:3773"
canonical_origin = "https://desktop.greyhound-chinstrap.ts.net/"
[storage]
path = "/home/will/.local/share/observatory"
max_stored_bytes = 0
max_live_artifacts = 0
[service]
teardown_timeout_ms = 30000
[client]
server = "http://127.0.0.1:3773"
timeout_ms = 30000
```

| Field | Type | Unit/range |
| --- | --- | --- |
| `server.listen` | string | loopback socket only |
| `server.canonical_origin` | string | absolute HTTPS origin, trailing `/` |
| `storage.path` | string | absolute supported local path |
| `storage.max_stored_bytes` | unsigned integer | bytes; 0 unlimited |
| `storage.max_live_artifacts` | unsigned integer | count; 0 unlimited |
| `service.teardown_timeout_ms` | unsigned integer | 1000..300000 ms |
| `client.server` | string | absolute HTTP(S), no credentials/fragment |
| `client.timeout_ms` | unsigned integer | 1..3600000 ms |

Environment is exactly `OBS_LISTEN`, `OBS_CANONICAL_ORIGIN`, `OBS_STORAGE`, `OBS_MAX_STORED_BYTES`, `OBS_MAX_LIVE_ARTIFACTS`, `OBS_TEARDOWN_TIMEOUT_MS`, `OBS_SERVER`, `OBS_CLIENT_TIMEOUT_MS`. Serve flags map to daemon fields. Client endpoint precedence is `--server`, `OBS_SERVER`, `client.server`, default.

Paths:

| Purpose | Path |
| --- | --- |
| Config | `${XDG_CONFIG_HOME:-$HOME/.config}/observatory/config.toml` |
| Data | `${XDG_DATA_HOME:-$HOME/.local/share}/observatory/` |
| Runtime/locks | `$XDG_RUNTIME_DIR/observatory/` |
| Versions/current | `$HOME/.local/lib/observatory/{versions,current}` |
| Stable command | `$HOME/.local/bin/obs` |
| Unit | `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/observatory.service` |

Behavioral defaults: Artifact 30 days; recovery seven days; normal history older than seven days and outside five newest; no ceilings; reserve greater of 1 GiB/5%; probes 60 seconds ±10%, deadline five seconds/no retry/no redirect/current two minutes; Service grace seven days/cleanup hourly; probe concurrency 16/2/1; teardown 30 seconds; client wait 30 seconds; systemd 5 seconds/max five per 60/grace 30; static cache one year immutable, shell/API no-store.

CLI exits are 0 success/replay/cancelled; 2 usage/validation/confirmation; 3 not-found/gone; 4 conflict/precondition; 5 daemon/destination unavailable/client timeout; 6 contention/maintenance/queue; 7 source changed; 8 trustworthy partial or zero-success fan-out; 9 trustworthy teardown failure/timeout; 10 capacity/unhealthy/internal/verification.

Stable errors include `already_exists`, `confirmation_required`, `precondition_required`, `changed_record`, `idempotency_conflict`, `idempotency_in_progress`, `client_timeout`, `contention`, `source_changed`, `teardown_failed`, `capacity`, `not_found`, `gone`, plus backup `unsafe_destination`, `unsupported_destination`, `capability_failed`, `destination_exists`, `staging_collision`, `destination_changed`, `destination_unavailable`, `verification_failed`, `backup_incomplete`, `catalogue_only`, `backup_completed`, `cancelled`, and setup `setup_precondition`, `provenance_invalid`, `ownership_conflict`, `unit_failed`, `health_timeout`, `daemon_identity_mismatch`, `schema_rollback_unsafe`, `tailscale_unavailable`, `serve_conflict`.

Storage health is `healthy|degraded|unhealthy|offline`; Service reachability is `online|offline|unknown|stale`; setup local health additionally may be `unavailable`. These do not conflate with command `ok`.

## 17. Implementation acceptance criteria

Implementation MUST ship shared fixtures through application service, HTTP API, CLI adapter, and applicable UI/local bootstrap adapter.

### Public control-plane fixtures

1. **Identity/URLs:** uppercase ID API `422`; stale browser slug/ID one `308`; unknown `404`; tombstone `410`; canonical-directory equivalence; only server-returned URLs; no client construction.
2. **Project lifecycle:** resolve unregistered allocates nothing; register once/conflict; move incomplete/changed enumeration atomic failure; successful new Project/Service IDs, old `410`, observations unknown, no teardown, Artifact association/URL unchanged; nonempty tombstone conflict; empty permanent gone.
3. **ETag/idempotency:** GET ETag; mutation missing `428`, stale `412`; new key once; same exact replay + header; changed fingerprint conflict; disconnect retry one effect; concurrent duplicate no duplicate.
4. **Batch:** mixed and zero-success both HTTP 200/`ok:true`/stdout/ordered, exact overall/partial/counts/exit 8; successful sibling remains; empty malformed is `ok:false` stderr/422/exit 2; per-entry import fields/retention override/warnings/duplicate/error/cleanup error asserted.
5. **Target URL corpus:** accept `https://host.example/`, loopback port/path, bracketed IPv6+ordinary query, encoded space; reject ftp, relative, userinfo including empty, fragment, every forbidden credential query case/encoding, invalid escape, raw backslash, control/CRLF/NUL, ports 0/65536, malformed host. Assert no rejected credential reaches probe/log/diagnostic.
6. **Teardown:** unavailable/missing directory/launch/nonzero/signal/default+override timeout/concurrent update preserve; only unchanged exit 0 tombstones; output bound/escape/truncation; known failure `ok:true` exit 9.
7. **Artifact lifecycle/capacity:** every retention transition/recovery/410/restore/early plan; live ceiling blocks create not replace; stored ceiling counts every defined Revision/staging state; reserve boundary; pressure protections; every capacity field.
8. **Browser security:** every allowed mutation with no-JS and JS parity; excluded mutations absent; GET inert; exact confirmation tier; stale ETag/token, wrong Host/Origin/Referer/Fetch Metadata, missing/expired/replay CSRF fail; success 303 canonical; Open distinctions; keyboard/focus/announcement/reflow/mobile/reduced-motion.
9. **Diagnostics/recovery:** unhealthy complete `ok:true` exit10; skipped partial exit8; no trustworthy context `ok:false`; preview changes no authority/bytes but writes plan/audit; stale/broadened apply rejects; one-intent resume; repair readable known schema gate; salvage record-validation outcomes; rebuild unrecoverable fields; full candidate validation; exact activate/restore/discard confirmations/loss/conflict/capacity/reference boundaries; backup normal/deep/catalogue-only.
10. **Adapter parity:** every multi-adapter mutation has same normalized operation/IDs/version/fingerprint/validation/audit/resource/URLs/error; no adapter-specific lifecycle.
11. **API inventory:** exercise every listed method/path, media/cache/schema/ETag/status/exit/pagination/cursor/filter/order; assert Artifact byte GET excluded; assert no setup/service-manager remote endpoint or alias. Configuration validation accepts TOML content, returns ordered parse/schema/semantic checks, mutates nothing, and rejects accidental precondition/idempotency requirements.
12. **CLI inventory:** snapshot every leaf/option/help/global selector/stdout/stderr/envelope/exit/confirmation/key behavior; exactly eight local leaves; all others daemon-only; `--server` rejected locally. Config validation requires a safe local regular FILE, sends no path, has no stdin mode, never activates, and fails exit 5 without daemon.

### Persistence, backup, and bootstrap fixtures

- Fault Publish/cleanup after every intent/transaction/copy/digest/sync/rename/visibility/tombstone phase and prove only complete bytes or explicit unavailable state.
- Backup accepts only btrfs/ext4/XFS after every probe step; rejects every listed unsupported class; races mount/parent/symlink/bind/final leaf; proves no-replace/no overwrite and 0700/0600.
- Concurrent Publish/replace/quarantine/cleanup during snapshot proves snapshot=lease=manifest=destination set; leased cleanup blocked exactly; corrupt workspace receipts never override SQLite.
- Fault backup after every snapshot/lease/create/write/digest/file sync/directory sync/manifest/marker/rename/parent sync/SQLite-completion boundary; inject short writes/EINTR/ENOSPC/quota/EIO/removal; only absent or one valid final bundle.
- Independently corrupt catalogue/Revision manifest/content/top manifest/marker, add/remove/link members, and require creation deep rehash. Freeze byte-identical format-v1 fixtures.
- Replay/cancel/resume at every backup phase; require ETag cancel, completed race semantics, 24-hour abandonment, exact evidence rehash, ambiguous staging no deletion, final backup no retention cleanup.
- Exhaust source/destination/restore capacity around exact reserves; budget live+candidates+rollback; reject unsupported/incomplete/changed/overlap restore; reverify private copy; detect loss/ID conflicts; crash before/after authority selection; explicit rollback only.
- Privacy snapshots assert no prohibited absolute path/metadata/content in remote API, manifest, audit, logs, diagnostics, errors; local CLI echoes only own destination.
- Empty-machine setup check creates nothing; invalid provenance/XDG/runtime/linger/foreign unit/port/root fails before effects; matching-unowned root not adopted; missing linger command not executed.
- First apply requires confirmation/key, bounded setup lock, exact staged/synced activation/config/unit/enable/start/health/Serve-last order; daemon alone creates data; repeated unchanged; changed key conflicts.
- External trust fixtures reject bare/substituted release and prove external verification plus self-check, checksum/provenance, target, link/digest activation, rollback version retention.
- Capability/dependency tests prove bootstrap cannot link/call/open domain/storage modules/files and queries only loopback health.
- Remote intended grant succeeds; ungranted tailnet, LAN, public fail; loopback unreachable; no Funnel; unrelated Serve unchanged.
- Service stop/start/restart/status semantics, SIGKILL startup phases, five/60 limit, bounded reset, unrelated unit behavior.
- Upgrade covers no migration, compatible migration, incompatible/uncertain rollback, daemon-only backup+migration, prior retained, no unsupported old binary.
- Foreign/partial states are never killed/adopted/overwritten. Remove/uninstall retain exact data/config sets, block drift, preserve unrelated handlers, and permit verified reinstall.

### Six implementation gates

1. **Adversarial walker races:** symlink swaps, renamed parents/children, mount crossings, unusual inode types, concurrent source changes; no escape/follow/unsafe copy/descriptor leak/overwrite.
2. **Crash/fault injection:** fail every transaction, sync, rename, WAL, migration, backup, cleanup, activation, shutdown; partial writes, I/O/full disk/busy/checkpoint; restart reaches valid state/gate.
3. **Every-connection SQLite policy:** every create/reuse/failure/replacement proves pinned engine, application/schema, foreign keys, WAL, FULL sync, bounded busy before use.
4. **Tokio load policy:** test initial four/four and evidence-approved reductions, bounded queues under reads/stream/Publish/cleanup/probes/cancel/shutdown; record CPU/RSS/threads/latency; never host defaults.
5. **systemd/package/Serve:** real user manager, linger, lifecycle, SIGTERM/SIGKILL, limits, journal, activation, health, migration backup, rollback, Serve preservation/conflict, tailnet outage, pinned Tailscale schema.
6. **Target ABI/static options:** released glibc binary on baseline/desktop, linkage, bundled SQLite/rustls; every other target unsupported pending equivalent matrix.

Failed gate is implementation work under this contract unless satisfying it requires changing a fixed decision.

## 18. Deferred work and unsupported configurations

Deferred: production implementation/install/deployment/current AGENTS onboarding; Sideshow bind/origin fix; producer adapters; application-specific Service release work; multi-user roles/auth; distributed storage; old-origin redirects; incompatible API v2; external release-signing identity/tooling execution; proof gates in section 17.

Unsupported: public/Funnel/LAN-only/non-tailnet; non-loopback backend; remote/cross-mount private storage; backup destinations outside probed btrfs/ext4/XFS; discovery/source links/crawls/live/move import; Service proxy/fallback/automatic runtime action/health claim; file browser/listing/SPA fallback/embedded previews; unmodified Sideshow 0.7.0 canonical use; musl/non-x86-64/untested Linux/macOS/Windows; containers/distro packages/Node/Bun/npm/external database/search/filesystem authority; manifests/workspaces/external backup as authority.

## 19. Install-time agent onboarding text

> **Install-time text — add once, and only once, Observatory is implemented and deployed.** Do not add this block to current agent instructions while `obs` is unavailable.

```markdown
### Observatory

The deployed Observatory service is the front door for browser-based agent work at <https://desktop.greyhound-chinstrap.ts.net/>.

- Start with `obs --help`, then use `obs <resource> --help` (for example, `obs artifact --help` or `obs service --help`) for the complete resource command inventory.
- Publish static browser-viewable work as Observatory-owned Artifacts with `obs artifact publish`; use explicit `obs artifact replace` only when advancing an existing Artifact.
- Register separately running interactive browser apps as Services with `obs service register`. A Service keeps its own behavior, state, runtime, exposure, and authorization.
- Use canonical URLs returned by the server. Never construct Observatory URLs from an origin, ID, slug, title, Project key, path, or Service name.
- Always pass a unique `--idempotency-key` for every mutation. Retry an uncertain request with the same key and identical arguments.
- Never proxy, embed, absorb, copy, or manage Service state through Observatory. Service Open uses the server-returned direct primary Target; `obs service remove` changes only the catalogue, and explicit `obs service teardown` is the sole optional external runtime action.
```

## 20. Decision traceability

| Decision | Coverage | Durable note/evidence |
| --- | --- | --- |
| [#2 Artifact contract](https://github.com/Whamp/observatory/issues/2) | §§3, 5, 7, 10 | Artifact resolution; `CONTEXT.md` |
| [#3 Service contract](https://github.com/Whamp/observatory/issues/3) | §§3–4, 6, 11 | Service resolution |
| [#4 Service transport validation](https://github.com/Whamp/observatory/issues/4) | §§6, 14, 17–18 | [Serve note](docs/research/2026-07-09-tailscale-serve-live-services.md); [probe](docs/research/evidence/2026-07-09-tailscale-serve-probe.md) |
| [#5 canonical address/trust](https://github.com/Whamp/observatory/issues/5) | §§1, 7, 14–16 | Trust resolution |
| [#6 routes/namespaces/slugs](https://github.com/Whamp/observatory/issues/6) | §§4, 7 | Route resolution |
| [#7 Artifact retention](https://github.com/Whamp/observatory/issues/7) | §§10, 12–13, 16–17 | Retention resolution; [persistence](docs/research/2026-07-09-observatory-persistence-architecture.md) |
| [#8 Service liveness](https://github.com/Whamp/observatory/issues/8) | §§6, 9, 11, 16–17 | Liveness resolution |
| [#9 CLI](https://github.com/Whamp/observatory/issues/9) | §§7–9, 16–17, 19 | [approved B CLI artifact](docs/prototypes/observatory-cli-contract.html?variant=B) |
| [#10 index/navigation](https://github.com/Whamp/observatory/issues/10) | §§9, 15, 17 | [approved B ledger artifact](docs/prototypes/observatory-index-navigation.html?variant=B) |
| [#11 persistence](https://github.com/Whamp/observatory/issues/11) | §§12–13, 15–17 | [persistence architecture](docs/research/2026-07-09-observatory-persistence-architecture.md) |
| [#12 stack/package/supervision](https://github.com/Whamp/observatory/issues/12) | §§15–18 | [stack note](docs/research/2026-07-09-observatory-implementation-stack.md) |
| [#15 import](https://github.com/Whamp/observatory/issues/15) | §§5, 7–8, 10, 12, 17–18 | Import resolution |
| [#16 diagnostics/recovery](https://github.com/Whamp/observatory/issues/16) | §§7–8, 12–13, 16–17 | [diagnostics/recovery](docs/research/2026-07-09-observatory-storage-diagnostics-recovery.md) |
| [#17 public control plane](https://github.com/Whamp/observatory/issues/17) | §§4–9, 16–17 | [public control plane](docs/research/2026-07-09-observatory-public-control-plane.md) |
| [#18 backup topology](https://github.com/Whamp/observatory/issues/18) | §§7–8, 12–13, 16–18 | [backup topology](docs/research/2026-07-09-observatory-backup-topology.md) |
| [#19 bootstrap authority](https://github.com/Whamp/observatory/issues/19) | §§3, 7–8, 14–18 | [bootstrap authority](docs/research/2026-07-09-observatory-bootstrap-authority.md) |

The map, glossary, every linked resolution, seven linked research notes, probe transcript, and two committed human-review artifacts are the durable source set. The first specification draft was superseded after independent handoff review; this revision incorporates all three reviews and closed decisions #17–#19 without reopening settled behavior.
