# Observatory product specification

Status: **decision-locked planning specification**
Canonical specification: this file
Canonical deployment origin: `https://desktop.greyhound-chinstrap.ts.net/`
Planning map: [issue #1](https://github.com/Whamp/observatory/issues/1)
Specification assembly: [issue #13](https://github.com/Whamp/observatory/issues/13)

## 1. Purpose, authority, and normative language

Observatory is the single known starting point for browser-based work produced or used by AI agents. It makes persistent static Artifacts and separately running browser Services findable from one private, tailnet-only front door.

This file is authoritative for Observatory product and implementation behavior. It synthesizes the closed decisions linked in the [traceability matrix](#20-decision-traceability). The linked issues, research notes, and prototypes retain rationale and evidence; they do not override this specification. A later change that conflicts with a MUST, MUST NOT, SHOULD, SHOULD NOT, or MAY here requires an explicit new product decision and an update to this file.

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** state normative requirements. Unqualified present-tense statements are also requirements where they define behavior. Examples use the canonical deployment origin unless marked illustrative. Server-returned URLs are authoritative even when an example shows their shape.

Production implementation and deployment are outside this planning map. A later implementation MUST satisfy this complete specification and its [acceptance criteria](#17-implementation-acceptance-criteria); this planning effort does not build, install, approve, or deploy the service.

## 2. Scope and planning boundary

### In scope

Observatory specifies:

- an owned catalogue of static Artifacts and immutable Revisions;
- a link-first catalogue of externally owned Services and their Targets;
- canonical browser routes, a versioned machine API, and an agent-first `obs` CLI;
- a Project-led human index with lifecycle, reachability, search, and detail controls;
- retention, probing, cleanup, storage diagnostics, backup, and recovery contracts;
- a SQLite-authoritative local persistence model and crash protocols;
- a private Tailscale Serve deployment and least-privilege tailnet authorization boundary; and
- the chosen Rust binary, embedded frontend, XDG layout, packaging, setup, and systemd user supervision model.

### Explicit non-goals

Observatory MUST NOT:

- reimplement, embed, absorb, rewrite, or proxy an external Service's UI, rendering, application logic, API, behavior, or state;
- discover Services from processes, listeners, ports, DNS, or network scans;
- start, restart, kill, signal, allocate ports for, supervise, or automatically decommission external Services;
- convert pi-annotate or Sideshow state into static reports or provide producer-specific snapshot/conversion adapters;
- act as a generic file browser, source-tree browser, directory listing, SPA host with fallback routing, or source-file watcher;
- expose public-internet access, Tailscale Funnel, or LAN-only/non-tailnet compatibility;
- provide multi-user accounts, application login, API keys, sessions, read/write roles, or per-Entry authorization;
- provide multi-host replication, distributed storage, external database/search infrastructure, or transparent compatibility proxying for arbitrary applications; or
- support production implementation or deployment as part of issue #1.

The unsupported and deferred boundary is listed fully in [section 18](#18-deferred-work-and-unsupported-configurations).

## 3. Domain model and authority

The terms below are exact and follow [`CONTEXT.md`](CONTEXT.md).

- **Entry**: a named browser destination discoverable through Observatory. An Entry is exactly one Artifact or one Service.
- **Artifact**: a static, persistent, browser-viewable bundle owned by Observatory. It is one regular file or one directory tree of regular files. Its stable identity selects one current Revision while live.
- **Revision**: an immutable published state of an Artifact. A successful replacement creates a Revision and atomically advances the Artifact's current selection.
- **Project**: a work context rooted at a canonical directory. The canonical directory is Project identity. A public Project ID/key is an address, not the identity claim.
- **Service**: a separately running interactive browser application referenced by Observatory while retaining its own behavior and state. Its identity is its name within one Project.
- **Target**: a named absolute browser URL through which a Service may be reached. Target names are unique within a Service. Every Service has exactly one primary Target and zero or more alternatives.
- **Teardown Action**: an optional Project-supplied executable and argument list that decommissions a Service only when explicitly requested.
- **Publish**: make an Artifact part of Observatory's owned collection under a stable identity.

Authority is deliberately split:

- SQLite is the sole catalogue authority for Projects, identities, metadata, current Revision selection, lifecycle state, Services, Targets, observations, tombstones, intents, leases, and audit events.
- Observatory owns copied Artifact bytes and their lifecycle.
- The Project supplies Project identity context and any Teardown Action.
- An external Service owns its process, behavior, state, exposure, TLS, authorization, and normal lifecycle.
- Tailscale grants authorize access to the Observatory origin. They do not transfer authorization to Service Targets.
- Recovery manifests, filesystem presence, and SQLite recovery output are evidence only. They never make an Entry live.

## 4. Project identity and moves

A Project MUST be identified by its resolved canonical directory. `-p, --project PATH` selects that directory and defaults to the CLI invocation's current working directory. The daemon, not the client, resolves the canonical path.

On first catalogue creation in that context, Observatory MUST allocate a non-secret 128-bit random Project ID and return a public key of the form `<project-slug>~<project-id>`. Project IDs are never supplied by callers, reassigned, or reused. Project display title and slug are presentation; the canonical directory remains identity.

Service names are unique within Project identity. Artifact Project association is catalogue metadata and does not enter Artifact serving URLs.

Observatory MUST NOT infer a directory move. A move requires explicit registration of a new Project identity/key, removal and re-registration of Services, and tombstoning of the old Project ID. The old Project MUST return `410 Gone`, with no redirect to the new Project. Artifacts MUST NOT be silently reassociated, and their serving URLs MUST remain unchanged. Losing a Project directory MUST NOT delete Observatory-owned Artifacts, including pinned Artifacts.

## 5. Artifact contract

### 5.1 Publish shapes, entry point, and media

Publish MUST accept exactly two source shapes:

1. one regular file, which is its own entry point; or
2. one directory tree of regular files, which is one Artifact.

Directory entry-point precedence is exact:

1. an entry supplied by the request;
2. `entry` in root `.obs.json`;
3. root `index.html`;
4. otherwise failure.

The entry path MUST be relative, stay inside the Artifact root, and name a regular file. Valid entry media types are `text/*`, `image/*`, `audio/*`, `video/*`, PDF, and JSON. Markdown is served as text. CSS, JavaScript, fonts, archives, and arbitrary binaries MAY be supporting files but MUST NOT be entry points. MIME type comes from the filename. Every byte response MUST include `X-Content-Type-Options: nosniff`.

Observatory MUST copy and serve bytes without compilation, rendering, transformation, fetching, inlining, or URL rewriting. Internal references SHOULD be relative. Root-relative references are unsupported; detectable instances SHOULD warn. Absolute external URLs are allowed and remain network-dependent.

Opening an Artifact or Revision base serves its declared entry point. A suffix MUST resolve to an actual file. There is no SPA fallback or directory listing. Hash-based browser routing remains valid.

### 5.2 Portable metadata

Root `.obs.json` is optional and recognized only for directory Artifacts:

```json
{
  "schemaVersion": 1,
  "entry": "artifact.html",
  "title": "Authentication flow",
  "description": "Review of the proposed sign-in architecture"
}
```

Only `schemaVersion`, `entry`, `title`, and `description` are permitted portable fields. `.obs.json` is consumed and MUST NOT be served. Project, slug, publication time, stable identity, retention, and Revision history belong only to the catalogue. Equivalent single-file metadata comes from request options.

Title precedence is request option, `.obs.json`, HTML `<title>`, then source basename. Description precedence is request option, `.obs.json`, then empty.

### 5.3 Validation and ownership

Publish MUST reject symbolic links, files with multiple hard links, sockets, devices, FIFOs, absolute or traversal paths, unreadable members, anything outside the root, and any non-regular leaf. Selected roots and members MUST be opened descriptor-relatively without following links. All regular files, including dotfiles, are copied except root `.obs.json`; there are no implicit ignore rules.

Contract violations block Publish: missing/invalid entry, malformed `.obs.json`, unsafe filesystem content, unreadable bytes, or unsupported entry media. Missing references, detectable root-relative URLs, unreachable external dependencies, and browser/JavaScript failures MAY warn but MUST NOT block. Observatory does not execute an Artifact to prove it renders.

There is no default file-count or byte-size limit. Insufficient capacity or configured operational ceilings MUST fail clearly before visibility.

Artifacts are trusted single-user content. JavaScript runs normally on the Observatory origin without per-Artifact sandboxing or isolation. Filesystem checks prevent accidental escape; they are not a hostile-publisher security boundary.

### 5.4 Create, replace, and immutable Revision semantics

`artifact publish` is strict creation. It allocates new never-reused Artifact and Revision IDs. Existing identity is never inferred from title, filename, source path, slug, bytes, or metadata.

`artifact replace ARTIFACT SOURCE` MUST name an existing Artifact. It creates a new immutable Revision and atomically advances the stable Artifact selection. The prior current Revision remains immutable and available until retention removes it. A failed or interrupted replacement leaves the current Revision and retention state unchanged.

Publish MUST stage, validate, checksum, and durably finalize a complete owned copy before catalogue visibility. Later source changes MUST NOT affect the owned copy. No failure or crash may expose a partial bundle.

### 5.5 Explicit import

`artifact import SOURCE...` is explicit migration intake into normal Publish, not discovery or another Artifact kind.

- Each ordered input explicitly selects one host-local regular file or directory. Relative paths resolve once against the caller's working directory. Observatory MUST NOT accept URL import, stdin archives, internal glob expansion, project-root crawling, watch mode, or conversion.
- A directory is one Artifact. Entry selection, metadata, validation, MIME, warnings, copy, retention, routes, and Revision semantics are identical to Publish.
- Project defaults to the selected CLI Project and MAY be overridden per import entry. It MUST NOT be inferred from source parents or `.obs.json`.
- Import always copies. It MUST NOT serve through, link, bind-mount, move, delete, or retain a live relationship to the source.
- Copying MUST compare file identity, type, size, timestamps, and link count before and after each read and retraverse directories before commit. Any addition, removal, rename, or metadata change fails that import entry as `source_changed`.
- A content fingerprint over entry path plus ordered relative paths, sizes, and digests MAY identify an advisory duplicate candidate. Equal content MUST NOT silently reuse or replace an identity.
- Batch commit is atomic per import entry, not across the batch. Results preserve input order and report `committed`, `failed`, or `unchanged_replay`; successful siblings survive mixed outcomes.
- Repeated normalized selection in one request MUST fail those repeated entries. Bulk request idempotency derives stable per-entry keys from the request key and position.
- Long-lived provenance records the import method, commit time, actor/request identity, and content fingerprint, but MUST NOT retain absolute source paths, home components, device/inode values, ownership, or permissions. Remote results use ordinal and basename unless the caller supplied a safe label.

## 6. Service contract

### 6.1 Identity, registration, and update

A Service is identified by `(Project canonical directory, Service name)`. The name MUST be immutable, normalized to Unicode NFC, non-empty after trimming, free of control characters, and compared case-sensitively as NFC UTF-8. Moving Project or renaming Service requires removal and strict re-registration, producing a new ID and leaving the old ID tombstoned. Target URL changes do not change Service identity.

Registration is explicit strict creation and MUST conflict on an existing identity. It commits without requiring reachability, starts every Target at `unknown`, and queues probes after commit. Update requires an existing Service and MAY atomically change Targets, presentation metadata, or Teardown Action, but MUST NOT change identity. Update or removal of a missing Service returns not found.

Removal deletes only catalogue metadata and observations. It MUST NOT affect the external process. Observatory MUST NOT discover Services automatically.

### 6.2 Targets and Open behavior

Every Service MUST have exactly one primary Target and MAY have alternatives. Each Target has a Service-local unique arbitrary name, an absolute credential-free HTTP(S) URL, and an optional label. `local` and `tailnet` are conventions with no hidden behavior. URLs MUST NOT contain userinfo, bearer tokens, signed query credentials, or credential fragments.

The primary Target is the configured default Open destination, not a health or deployment claim. Alternatives appear in details. Observatory MUST NOT select a Target based on probes or silently fall back. Removing the primary MUST nominate its replacement in the same atomic update.

Service representations return distinct `detailUrl` and direct `primaryTargetUrl` values. Service Open MUST navigate directly to `primaryTargetUrl`; Observatory MUST NOT proxy, rewrite, wrap, or derive it.

### 6.3 Teardown Action and runtime authority

A Service MAY store one Teardown Action as an executable plus argv, never an implicit shell string. It runs only after an explicit confirmed teardown request, with the Project canonical directory as working directory and a bounded timeout.

Exit `0` removes only the unchanged Service record. Missing Project directory, launch error, nonzero exit, timeout, or concurrent record mutation preserves the Service and reports captured output safely. Record-version comparison or serialization MUST prevent an old action from deleting a changed record. Missing action reports teardown unavailable; normal removal remains possible.

Teardown is the sole permitted external runtime effect. Automatic expiry and all other Observatory actions MUST NOT invoke it.

## 7. Routes, names, and canonical URLs

### 7.1 Namespace allocation

| Namespace | Contract |
| --- | --- |
| `/` | Permanent front door; `308 Permanent Redirect` to `/ui/`. |
| `/ui/…` | Unversioned human ledger, Project, detail, and control pages. |
| `/api/v1/…` | Versioned machine API. Compatible additions are allowed; existing meanings and URL semantics are stable. |
| `/_static/<build-id>/…` | Immutable embedded UI assets. |
| `/artifacts/<artifact-key>/…` | Stable-current Artifact bytes. |
| `/revisions/<revision-id>/…` | Immutable Revision bytes. |

These six first segments are reserved. Unknown top-level routes return `404 Not Found`. `/api` and unversioned API routes MUST NOT redirect. Resource types remain explicit; there is no generic `/entries/` route.

Project, Service, and Artifact route keys are `<slug>~<id>`. Revision routes contain only the ID. Project, Service, Artifact, and Revision IDs are independently generated random 128-bit values encoded as exactly 26 lowercase Crockford-base32 characters. IDs are opaque, caller-independent, collision-retried before visibility, never reassigned, and never reused.

Issued terminal IDs retain durable tombstones with at least ID, type, and terminal state. Formerly valid URLs return `410 Gone`; unknown or never-issued IDs return `404 Not Found`.

### 7.2 Slugs

A caller MAY suggest a slug; otherwise it derives from Project directory basename, Service name, or Artifact title/source fallback. Normalization is exact:

1. Unicode NFKD;
2. discard combining marks and non-ASCII characters not transliterated by decomposition;
3. lowercase ASCII;
4. replace each maximal run outside `a-z0-9` with one `-`;
5. collapse and trim `-`; and
6. truncate to 48 ASCII characters without trailing `-`.

Stored grammar is `[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?`, including a one-character alphanumeric. Empty caller-supplied normalization is rejected; empty automatic derivation becomes `project`, `service`, or `artifact`. `~` never belongs to a slug. Bare-slug collisions are allowed because IDs disambiguate.

The creation slug remains stable after title/label changes. Explicit slug rename preserves ID. Any syntactically valid stale or noncanonical slug paired with the same live ID receives `308` to the current key. There are no aliases or redirects between IDs.

### 7.3 Artifact paths and request canonicalization

Under `/artifacts/<artifact-key>/` and `/revisions/<revision-id>/`, every suffix is an Artifact-relative bundle path. No child segment is reserved. Paths are decoded exactly once as UTF-8 by segment. Malformed escapes, encoded `/` or `\`, NUL, `.`/`..`, traversal, double decoding, and duplicate separators are rejected. Canonical paths leave RFC 3986 unreserved bytes literal and percent-encode all other UTF-8 bytes with uppercase hexadecimal. Lookup is case-sensitive against the manifest and never filesystem-dependent.

UI collection/detail and Artifact/Revision base URLs end in `/`; actual file URLs do not. For `GET`/`HEAD`, safely identifiable missing/extra slash, uppercase ID, stale slug, or encoded key character receives one absolute `308`. API routes are exact and do not depend on redirects.

Redirects preserve query strings. Queries do not establish identity or file lookup; Artifact JavaScript MAY interpret them. Fragments never reach the server. Returned canonical identity URLs omit query and fragment unless the caller supplied separate application navigation state.

### 7.4 Representative absolute URLs

```text
Front door
https://desktop.greyhound-chinstrap.ts.net/

UI
https://desktop.greyhound-chinstrap.ts.net/ui/

Project
https://desktop.greyhound-chinstrap.ts.net/ui/projects/observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p/

Artifact details
https://desktop.greyhound-chinstrap.ts.net/ui/projects/observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p/artifacts/auth-flow~6r3t8w2y5b9c4d7f0g1h3j5k7m/

Stable Artifact and supporting file
https://desktop.greyhound-chinstrap.ts.net/artifacts/auth-flow~6r3t8w2y5b9c4d7f0g1h3j5k7m/
https://desktop.greyhound-chinstrap.ts.net/artifacts/auth-flow~6r3t8w2y5b9c4d7f0g1h3j5k7m/api/v1/_static/client.js

Immutable Revision
https://desktop.greyhound-chinstrap.ts.net/revisions/7s4v9x3z6c0d5f8g1h2j4k6m8n/

Service details and separately returned direct Target
https://desktop.greyhound-chinstrap.ts.net/ui/projects/observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p/services/pi-annotate~8t5w0x4z7c1d6f9g2h3j5k7m9n/
https://desktop.greyhound-chinstrap.ts.net:8443/session/current

API and UI asset
https://desktop.greyhound-chinstrap.ts.net/api/v1/artifacts/6r3t8w2y5b9c4d7f0g1h3j5k7m
https://desktop.greyhound-chinstrap.ts.net/_static/sha256-8f31c2/app.js
```

Every create/get/update response MUST return canonical absolute URLs. Clients MUST use them rather than construct routes from origin, IDs, keys, slugs, paths, titles, or names.

## 8. Agent CLI contract

The approved grammar is **B — Resource namespaces**:

```text
obs [GLOBAL OPTIONS] <RESOURCE> <ACTION>
```

`obs --help` MUST list all six namespaces. `obs <resource> --help` MUST list that resource's complete capability inventory, runnable leaf synopses, and examples. Parent help exits `0`. Resource/action inventory is:

```text
obs artifact
  publish SOURCE
  replace ARTIFACT SOURCE
  import SOURCE...
  list [--all]
  show ARTIFACT [--revisions]
  remove ARTIFACT
  restore ARTIFACT [--ttl DURATION|--pin]
  pin ARTIFACT [--reason TEXT]
  unpin ARTIFACT [--ttl DURATION]

obs service
  register NAME --target NAME=URL... [--primary NAME]
  update SERVICE
  list
  show SERVICE
  remove SERVICE
  teardown SERVICE
  refresh SERVICE | refresh --all
  pin SERVICE [--reason TEXT]
  unpin SERVICE
  keep SERVICE
  target list SERVICE
  target show SERVICE TARGET
  target add SERVICE NAME=URL [--primary]
  target update SERVICE TARGET --url URL [--label TEXT]
  target remove SERVICE TARGET [--new-primary TARGET]
  target promote SERVICE TARGET
  target refresh SERVICE TARGET

obs project
  list
  show PROJECT
  resolve [PATH]

obs cleanup
  preview [--pressure]
  run --yes [--pressure]

obs system
  status
  diagnostics [--deep]
  setup check
  setup apply --yes
  recovery preview OPERATION [SELECTOR...]
  recovery apply PLAN --yes
  recovery resume [OPERATION]
  backup create DESTINATION
  backup verify BACKUP [--deep]

obs serve
  [--listen 127.0.0.1:3773]
  [--canonical-origin URL]
  [--storage PATH]
```

The diagnostics decision extends the approved `system` noun without changing the B grammar. Recovery `OPERATION` is one of `reconcile`, `quarantine`, `repair_catalogue_candidate`, `salvage_catalogue_candidate`, `rebuild_catalogue_candidate`, `validate_candidate`, `activate_candidate`, `restore_backup`, or `discard`.

Global options are `--server URL`, `-p, --project PATH`, `--json`, `--timeout DURATION`, and `--idempotency-key KEY`. Destructive mutations also accept `--yes`. Project defaults to current working directory; the daemon resolves it. `project resolve [PATH]` returns canonical directory and Project key.

### Output, errors, and idempotency

Human success goes to stdout. Warnings, progress, logs, and diagnostics go to stderr. With `--json`, success writes exactly one JSON value to stdout; failure leaves stdout empty and writes exactly one JSON error to stderr. The stable envelopes are:

```json
{"schemaVersion":1,"ok":true,"result":{}}
{"schemaVersion":1,"ok":false,"error":{"code":"contention","message":"catalogue remained busy for 5s","retryable":true,"details":{"retryAfterMs":250}}}
```

Batch/bulk results preserve order, report each outcome, and set `partial:true` when mixed. Successful resource results include opaque IDs and server-returned URLs.

Every mutation MUST accept an idempotency key; agents and automation MUST always provide one. A key binds to a canonical request fingerprint. Same key and fingerprint returns or resumes the recorded result; changed input conflicts; committed effects never repeat. A client timeout after dispatch reports unknown commit state and directs an identical retry with the same key.

A human TTY MAY prompt once for a destructive command. Non-TTY invocation MUST NOT prompt and returns confirmation-required unless `--yes` is present. Confirmation is required for Artifact removal, Service removal, Service teardown, primary Target removal, cleanup run, setup apply, catalogue replacement, availability-changing quarantine, restore, and permanent discard.

### Daemon boundary

The CLI sends every read and mutation to the backend. It MUST NOT open SQLite or storage. `obs serve` is the sole backend write authority, binds loopback, owns validation and scheduled work, and refuses a second daemon before writes. Normal commands MUST NOT auto-start it. `system setup` alone may reconcile Observatory's owned Serve root handler.

## 9. Human index and navigation

The approved UI is **B — Project ledger**, with C-style fast search only as a secondary affordance.

- The first navigation level is All Projects, then each Project name with canonical directory context. All Projects is default.
- The second level is one dense ledger of Artifacts and Services in scope. Default order is most recent observation or Publish first; Title and Needs attention are alternatives.
- Each row shows kind/accession cue, title, description, lifecycle or reachability, size/Revision or diagnostics, recency, Project, and separate Open and Details actions.
- Artifact rows expose current, pinned, expiring, and recoverable states; retention mode/deadline or recovery warning; current Revision; logical size; Project; and Publish recency. Recoverable stable URLs remain `410` until restore.
- Service rows expose the primary Target's online, offline, unknown, or stale host-vantage state, observation age, HTTP status and duration or categorized transport failure, and all-offline expiry deadline. They never label reachability as application health.
- Artifact Open uses the stable `/artifacts/<artifact-key>/` URL. Service Open leaves Observatory and opens the primary Target directly. Details stays in Observatory. Alternatives appear only as named choices in Service details.
- Search stays within current Project scope and covers title, description, kind, and visible state/diagnostic terms. Kind filters are All, Artifacts, and Services. Search MUST NOT replace Project navigation or hide lifecycle, liveness, recency, retention, or diagnostics.
- The catalogue MUST NOT execute or embed previews.

At mobile widths, Project navigation becomes a horizontally scrollable strip and ledger rows become stacked cards. Cards MUST preserve the desktop state, lifecycle/diagnostic, recency, Open, and Details information.

The baseline MUST support semantic landmarks and headings, keyboard navigation and visible focus, a skip link, programmatic labels, status announcements that do not steal focus, sufficient contrast, viewport reflow without lost actions, and `prefers-reduced-motion`. Core Project navigation, ledger state, and links MUST work without JavaScript; JavaScript progressively enhances search, filtering, ordering, and forms.

## 10. Artifact retention, cleanup, and audit

Every Artifact has exactly one mode:

| Mode | Required behavior |
| --- | --- |
| Expiring | Default. Expire 30 days after latest successful Publish/replacement. Views do not renew it. |
| Explicit TTL | Positive request duration from successful Publish; replacement restarts the same duration unless changed. |
| Pinned | No deadline until explicit unpin/delete. Unpin selects a positive TTL or starts the 30-day default at unpin time. |

Retention belongs to Artifact identity. Failed Publish does not change it. Times are stored as absolute UTC instants. Pin reason is optional visible metadata. Pinning protects the Artifact and current Revision, not unlimited history.

At deadline an Artifact is expired even before cleanup. It leaves normal discovery and its stable URL returns `410`. Expiry and normal explicit deletion start a seven-day recovery window. Restore chooses pinned, explicit TTL, or default retention. After seven days cleanup may purge bytes and Revisions while retaining tombstone identity, Project, title, expiration/deletion time, cause, and reclaimed byte count. Early purge is a separately previewed, exact, confirmed destructive operation.

A superseded Revision is normally eligible only when older than seven days **and** outside the five most recent superseded Revisions. Pressure cleanup MAY remove older superseded Revisions inside those windows, oldest first, but MUST preserve every live current Revision. Removed Revision URLs return `410` and IDs are never reused.

There are no default byte, file, live-Artifact count, or global storage ceilings. Deployments MAY configure global stored-byte and live-Artifact count ceilings. Observatory MUST reserve the greater of 1 GiB or 5% of the storage filesystem as operational free space.

Before a Publish that would breach a ceiling/reserve, cleanup order is:

1. abandoned staging;
2. normally eligible superseded Revisions;
3. expired/deleted Artifacts past recovery;
4. additional superseded Revisions oldest first, preserving live current Revisions.

Cleanup MUST NOT shorten recovery or evict a live Artifact. Insufficient safe capacity fails before commit with required bytes, available capacity, blocking limit/reserve, and reclaimable bytes. Existing reads continue.

Preview lists candidates, reasons, Revisions, bytes, and recoverability. Catalogue/UI diagnostics show retention, deadline/pin, recoverability, logical size, Revision count, cleanup errors, aggregate use, limits, reserve, reclaimable bytes, and latest run.

Every expiry, restore, retention mutation, Revision removal, deletion, purge, pressure cleanup, and failure appends an audit event with timestamp, actor (`operator` or `system`), cause, IDs, and byte result. Cleanup is a single-writer, restart-safe, idempotent intent state machine. It marks unavailable, atomically quarantines complete directories on the same filesystem, finalizes tombstones, and removes bytes asynchronously. One candidate's failure remains retryable and does not block independent candidates. Persistent failure or reserve breach makes storage unhealthy and blocks byte-adding writes while intact reads continue.

## 11. Service reachability, expiry, and concurrency

Observatory probes each Target from the backend host/network namespace with direct HTTP(S) `GET`, no ambient proxy, no redirect following, and a five-second deadline through response headers. Any HTTP response means reachable; DNS, connection, TLS, protocol, and timeout errors mean unreachable. Probes do not claim application health.

All Targets are scheduled every 60 seconds with up to 10% jitter, once after startup, and immediately after committed registration/relevant update. Writes do not wait. Page views never probe. Target state for its current URL/version is:

- `unknown`: no completed observation;
- `online`: response observation no older than two minutes;
- `offline`: failure observation no older than two minutes; or
- `stale`: latest observation older than two minutes, retaining the labeled underlying result.

Scheduler/internal failures do not create offline facts. Late prior-version results are discarded. URL change resets that Target to unknown; add starts unknown; rename is delete/add; delete removes observation. Unchanged URLs preserve observations.

Services are unpinned by default. Seven-day expiry grace starts only when every current Target has a current-version offline observation and none is online. Online resets it. Unknown/stale neither starts nor completes expiry. Target mutation, explicit keep, or unpin starts a fresh seven-day grace when still all-offline. Pin suppresses automatic expiry but not probes. Reachable Services have no age-only lease.

Cleanup runs at least hourly. At deadline it freshly probes every current Target under normal bounds and deletes catalogue data only when all finish offline and the record version is unchanged. Reachability, internal failure, unknown, concurrent mutation, or incomplete confirmation preserves the Service. Automatic deletion never invokes teardown.

Manual refresh supports one Target, one Service, or all Services and returns ordered per-Target partial outcomes. Global concurrency is 16, per destination host is two, and per Target is one. Duplicate work coalesces. Scheduled, startup, manual, and cleanup probes share these bounds. One Target cannot block writes or independent results.

Service details MUST show every Target URL, state, timestamp, duration, HTTP status or DNS/connection/TLS/protocol/timeout category, host vantage, pin, last register/update/keep activity, and expiry deadline. URLs and diagnostics MUST NOT expose credentials.

## 12. Persistence and crash protocols

### Authority and exact storage layout

SQLite is authoritative. The exact private durable root is:

```text
catalogue.sqlite
staging/<operation-id>/
revisions/<opaque-revision-id>/
quarantine/<operation-or-revision-id>/
backups/<backup-id>/
candidates/<candidate-id>/
```

SQLite owns all visibility and lifecycle state. Immutable Revision directories contain served bytes plus one reserved non-served recovery manifest with schema, Artifact/Revision IDs, entry path, counts, Publish instant, and ordered file path/size/digest records. SQLite stores the manifest digest.

Database and byte paths use generated opaque IDs, never mutable or source-derived values. Catalogue, sidecars, staging, Revisions, quarantine, backups, and candidates MUST share one supported local filesystem wherever atomic rename is required. Remote/cross-mount layouts are unsupported.

Use `STRICT` tables, constraints, foreign keys on every connection, WAL only on local storage, `synchronous=FULL`, bounded busy handling, short `BEGIN IMMEDIATE` writes, indexes for decided lookup/due-work paths, and compare-and-swap record versions. Keep only the latest Target observation.

### Publish protocol

1. Preflight validation/capacity; allocate IDs; commit durable staging intent without visibility.
2. Descriptor-relatively copy, validate, checksum, write manifest, and sync every file/directory in same-filesystem staging.
3. Atomically rename to final Revision and sync the parent.
4. In one short SQLite transaction, verify intent/version, insert Revision/audit state, update current selection and retention, and commit visibility.

Startup MAY complete only an intent whose final bytes, IDs, manifest, and catalogue expectations match. It MAY resume intact staging. It MUST quarantine malformed, mismatched, or unreferenced bytes rather than adopt them.

### Cleanup protocol

1. Transactionally recheck eligibility/pin/current/version, append intent/audit, and mark the Revision unavailable.
2. Atomically rename its complete directory to quarantine and sync both parents.
3. Transactionally finalize tombstone and reclaimed-byte state; unlink asynchronously and idempotently.

Missing bytes are an error, not evidence of completed deletion.

### Backup, migration, rebuild, and repair boundaries

A complete backup leases the exact committed Revisions, takes a consistent SQLite Online Backup snapshot, copies leased immutable directories, binds snapshot/schema/application ID and exact digests in a top-level manifest, syncs/finalizes/verifies, then releases the lease. Copying only `catalogue.sqlite` while WAL is active is not a complete backup.

Migrations are ordered, transactional, monotonic, exclusive-lock protected, and fail closed. Observatory sets application ID/user version, refuses newer unknown schema, creates and verifies a complete pre-migration backup, updates version transactionally, then runs foreign-key and quick checks before writes.

Repair, SQLite recovery salvage, and manifest rebuild MUST produce separate candidates and loss/ambiguity reports. They MUST NOT mutate the only copy or become authority automatically. Rebuild cannot recover current selection, retention/pins, tombstones, audit history, Services, Targets, teardown, or observations from manifests alone. Activation requires full validation, maintenance lock, exact preview, confirmation, atomic authority selection with WAL sidecars, retained rollback material, and audit.

## 13. Storage diagnostics and recovery

### Profiles and health

| Profile | Required checks and effect |
| --- | --- |
| `system status` | Fast, read-only: filesystem/support, capacity/reserve, SQLite open/error/application/schema, WAL presence/size/checkpoint failure, counts/ages of intents/staging/quarantine/leases/cleanup failures, and live-current Revision path existence. Never waits behind long work. |
| `system diagnostics` | Online read-only bounded snapshot: fast checks plus `quick_check`, `foreign_key_check`, passive checkpoint observation, all catalogue paths, manifest parse/version/digest, intents, staging/quarantine, leases, cleanup details. No full content hashing. |
| `system diagnostics --deep` | Maintenance-gated: normal checks plus `integrity_check`, every path/size/digest, owned orphan scan, named backup verification, and disposable filesystem sync/rename capability probe. Catalogue reads, mutations, probes, and Artifact serving fail explicitly during the gate; external Services are untouched. |

`ok` reports command execution; `result.health` reports `healthy`, `degraded`, `unhealthy`, or `offline`. Requested checks return ordered entries with `id`, `status` (`pass|warn|fail|error|skipped`), stable `state`, `category`, `message`, `retryable`, `scope`, timestamps/duration, and redacted details. Partial/skipped work sets `partial:true`. Diagnostics exit `0` for complete healthy/degraded, `8` for trustworthy partial, and `10` for completed unhealthy.

Required check IDs and stable states are exact:

| Check | Stable states |
| --- | --- |
| `sqlite.open` | `open`, `not_found`, `permission_denied`, `busy`, `io_error`, `not_database` |
| `sqlite.application` | `matches`, `mismatch`, `unreadable` |
| `sqlite.schema` | `supported`, `older_migration_required`, `newer_unsupported`, `invalid` |
| `sqlite.quick` / `sqlite.integrity` / `sqlite.foreign_keys` | `ok`, `violations`, `not_run` |
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

Stable storage categories are `catalogue`, `schema`, `integrity`, `wal`, `content`, `operation_interrupted`, `missing_bytes`, `quarantine`, `lease`, `cleanup`, `filesystem`, `capacity`, `contention`, `permission`, and `internal`. Service reachability remains separate from storage health. The [diagnostics decision note](docs/research/2026-07-09-observatory-storage-diagnostics-recovery.md#check-taxonomy) retains the detailed mapping and rationale.

Fail-closed gates are exact:

- unavailable/wrong/new/invalid/corrupt catalogue or unsupported filesystem blocks catalogue writes/probes and exposes no Entries from untrusted authority;
- missing/corrupt/invalid Revision bytes disables affected Revisions and blocks byte-adding writes pending reconciliation;
- interrupted Publish/cleanup blocks byte additions until classified;
- reserve breach/persistent cleanup failure blocks byte additions but permits healthy metadata, cleanup/recovery, probes, and intact reads;
- checkpoint contention alone is degraded/retryable; WAL I/O/corruption blocks writes; and
- stale leases block cleanup only for named Revisions.

### Exact plan/apply contract

Recovery preview is read-only and creates a durable plan containing plan ID, exact identities/digests, health generation, operation, scope/effect/availability, estimated bytes, ambiguity/loss report, preconditions, confirmation, rollback point, and expiry. Apply accepts only that unexpired plan, rechecks every fingerprint/precondition, never broadens scope, and rejects changed state. Resume continues an existing nonterminal intent only.

Every mutation runs through the daemon, records intent before effects, binds idempotency fingerprint, uses a global authority lock or per-Revision lock as appropriate, is crash-resumable, and appends audit. Missing paths are evidence only. Restart preserves old or new authority, never half-selected authority.

`reconcile` alone is automatic at startup and only follows matching intents. `quarantine` preserves evidence and marks committed bytes unavailable before moving them. Repair/salvage/rebuild create candidates. `validate_candidate` cannot waive failures. `activate_candidate` and `restore_backup` require full validation, offline maintenance, exact preview, `--yes`, authority-last cutover, and retained rollback material. `discard` is the sole irreversible storage operation and cannot remove active/only catalogue, live current Revision, active candidate/lease, or nonterminal-operation evidence.

Audit covers health-gate changes, plans, all operation phases, reconciliation, quarantine, backup leases, candidates, activation/rollback, cleanup/discard, actor, IDs, before/after digests, bytes, errors, and durability phase. Diagnostics and logs redact absolute private paths, teardown argv, secret-bearing URLs, SQL/page/file content, and recovery rows.

## 14. Network and security boundary

Observatory is private and tailnet-only. The configured canonical HTTPS origin defaults to `https://desktop.greyhound-chinstrap.ts.net/`; port 443 is recommended and omitted. Another MagicDNS host/HTTPS port is a deployment migration; non-443 origins include the port. Old-origin redirects are not required.

The backend MUST bind loopback only. The local port is independently configurable and never appears in public Observatory URLs. Setup MUST verify exact active Serve host/port and reject or prominently diagnose mismatch; it MUST NOT guess.

The deployment owns only the root Serve handler for the canonical host/port, proxying to the loopback backend. Setup inspects state, refuses unrelated conflicts, preserves unrelated handlers, and verifies the result. Ordinary requests/startup only read Serve state. Teardown removes only a handler whose ownership fingerprint still matches. Observatory never owns Service Target handlers.

Tailscale is the sole remote authentication boundary, but tailnet membership alone is insufficient. Deployment MUST use an explicit least-privilege Tailscale grant for intended operator identities/agent devices to the Observatory node and HTTPS TCP port. Will's identity is the default human principal; tagged/headless agents need explicit grants. Authorized principals receive the full Observatory capability set.

There are no Observatory accounts, login, cookies, password database, API keys, or roles. Backend loopback prevents LAN/tailnet bypass and forged Serve headers. Serve identity headers MAY support attribution but are not second authorization; local host processes are inside the machine boundary. Browser mutation routes MUST validate canonical Host and same-origin requests.

Tailscale Funnel and public exposure are prohibited. LAN-only and non-tailnet clients are unsupported. Enabling Tailscale HTTPS exposes the certificate name through Certificate Transparency; this is accepted.

Service Targets are independent direct origins. Their operator owns grants, TLS, authentication, ports, and proxy correctness. A root-oriented Service MUST use its own root Serve mount, not a path below Observatory. Before calling a Service URL canonical, application-specific release validation MUST prove loopback bind, root routes, correct public HTTPS origin/forwarded handling, live transport from host and remote tailnet client, and persistent Serve restart/rollback behavior. Unmodified Sideshow 0.7.0 fails loopback bind and external-origin handling and MUST NOT be the canonical example.

## 15. Implementation, packaging, and supervision

Observatory MUST be one Rust Cargo binary, `obs`, using:

- Tokio + axum;
- clap;
- serde;
- rusqlite with pinned bundled upstream SQLite and backup support;
- an Observatory-owned rustix Linux filesystem deep module; and
- reqwest with default features disabled and rustls/web-PKI roots.

The binary embeds the Project-led HTML/CSS/ES-module frontend at compile time. There is no Node, Bun, npm, SPA framework, client router, or production frontend runtime. `/_static/<build-id>/` uses a full-asset content build ID, immutable one-year caching, and strong ETags. UI shell/API use `Cache-Control: no-store`. Server-rendered core navigation works without JavaScript.

`obs serve` uses four Tokio workers, at most four blocking threads, bounded filesystem/SQLite queues, and independently bounded probes. The daemon lock is `$XDG_RUNTIME_DIR/observatory/daemon.lock`. The default backend is `http://127.0.0.1:3773`; `--server` remains authoritative and normal commands never auto-start the daemon.

Release only pinned, locked `x86_64-unknown-linux-gnu` initially. The declared desktop baseline is kernel 7.0.14, glibc 2.43, Rust 1.96.1, systemd 261, Tailscale 1.98.8, 60 GiB RAM, and local btrfs home storage; the build's glibc baseline MUST be no newer than 2.43. A release includes stripped binary, separate symbols, SHA-256, signed provenance, SBOM, licenses, and passing formatting, Clippy, tests, dependency/vulnerability, and cargo-deny checks. Install versions atomically under XDG-adjacent user paths, select through `current`, and keep the previous version through health verification.

`system setup apply --yes` alone installs/updates the user unit, reloads/enables/starts it, verifies loopback health, then owns the one Serve root after conflict checks. Missing linger is a failed precondition with an operator command; Observatory MUST NOT run sudo/loginctl. Unrelated failed units do not block Observatory.

The generated systemd user unit policy is exact:

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

The unit MUST log to journald. It MUST omit `ProtectHome`, `PrivateTmp`, syscall filters, `DynamicUser`, restricted working directory, and `ProtectSystem=strict`, because those conflict with Project/source access required by Publish and Teardown. The [stack decision note](docs/research/2026-07-09-observatory-implementation-stack.md#systemd-user-supervision) retains the rationale.

Startup order is configuration; private runtime/lock; storage/SQLite classification; application/schema validation and backup-gated migration; intent reconciliation; health gates; loopback bind/readiness; background workers; read-only Serve state. `/api/v1/system/health` appears only after classification/reconciliation and reports build/API, storage, migration, recovery, worker, and Tailscale state without private paths.

Tailscale failure is degraded, not fatal. Untrusted catalogue authority is offline and exposes diagnostics but no Entries. SIGTERM stops dispatch, finishes short transactions, cancels probes, leaves resumable long intents, checkpoints when bounded/safe, closes listener/database, and exits within 30 seconds. A clean stop does not restart; failures follow the bounded restart policy.

Update activation is atomic and health-checked. Pre-migration failures MAY roll back the executable. Post-migration automatic executable rollback is allowed only when the prior binary supports the resulting schema. Data restore is always a separate previewed, confirmed offline recovery. Structured stderr logs go only to journald and follow diagnostics redaction.

## 16. Configuration, defaults, errors, and health

Configuration precedence is field-by-field: CLI, environment, `config.toml`, built-in default. Configuration is secret-free and does not live-reload; changes require restart. `SIGHUP` reports reload unsupported and changes nothing.

### Paths and configuration

| Purpose | Exact value/default |
| --- | --- |
| Config | `${XDG_CONFIG_HOME:-$HOME/.config}/observatory/config.toml` |
| Data root | `${XDG_DATA_HOME:-$HOME/.local/share}/observatory/` |
| Runtime | `$XDG_RUNTIME_DIR/observatory/` (`0700`; XDG runtime must be absolute, user-owned, protected) |
| Daemon lock | `$XDG_RUNTIME_DIR/observatory/daemon.lock` |
| Versions | `$HOME/.local/lib/observatory/versions/<version>/obs` |
| Active selector | `$HOME/.local/lib/observatory/current` |
| User command | `$HOME/.local/bin/obs` |
| systemd unit | `${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/observatory.service` |
| Daemon fields | `OBS_LISTEN`, `OBS_CANONICAL_ORIGIN`, `OBS_STORAGE` |
| Client endpoint precedence | `--server`, `OBS_SERVER`, `client.server`, `http://127.0.0.1:3773` |

Relative XDG/storage paths and non-loopback listeners are invalid. Storage directories are `0700` with no group/other access.

### Behavioral defaults

| Setting | Default/required value |
| --- | --- |
| Canonical origin | `https://desktop.greyhound-chinstrap.ts.net/` |
| Backend/listen | `http://127.0.0.1:3773` / `127.0.0.1:3773` |
| CLI client timeout | 30 seconds |
| Artifact retention | 30 days |
| Artifact recovery window | 7 days |
| Normal superseded history | older than 7 days and outside 5 newest superseded Revisions |
| Size/file/Artifact ceiling | none by default |
| Free-space reserve | greater of 1 GiB or 5% filesystem |
| Probe schedule | every 60 seconds, up to 10% jitter |
| Probe deadline/retry/redirect | 5 seconds / none / do not follow |
| Current observation age | at most 2 minutes |
| Service all-offline grace | 7 days |
| Service cleanup | at least hourly |
| Probe concurrency | 16 global, 2/destination host, 1/Target |
| Tokio concurrency | 4 workers, at most 4 blocking threads |
| systemd restart | 5 seconds; max 5 starts/60 seconds |
| Graceful stop | 30 seconds |
| UI static cache | 1 year, immutable; shell/API `no-store` |

### CLI exit and error categories

| Exit | Stable meaning/examples |
| ---: | --- |
| 0 | success, including unchanged idempotent replay |
| 2 | usage, validation, unsafe input, `confirmation_required` |
| 3 | `not_found` or `gone` |
| 4 | strict-create/idempotency/record `conflict`, including `already_exists` or `changed_record` |
| 5 | daemon `unavailable` or `client_timeout` |
| 6 | bounded `contention` exhausted |
| 7 | `source_changed` |
| 8 | ordered `partial` batch/probe/diagnostic result |
| 9 | `teardown_failed` or teardown timeout; Service preserved |
| 10 | `capacity`, unhealthy storage, or completed diagnostics reporting unhealthy |

Errors MUST keep stable code, human message, retryable boolean, and structured details. At minimum, settled codes include `already_exists`, `confirmation_required`, `client_timeout`, `contention`, `source_changed`, `teardown_failed`, `capacity`, `not_found`, `gone`, and `changed_record`.

Storage health is exactly `healthy`, `degraded`, `unhealthy`, or `offline`, separate from command `ok`. Service reachability is exactly `online`, `offline`, `unknown`, or `stale` and MUST NOT be conflated with storage health or application health.

## 17. Implementation acceptance criteria

Implementation acceptance requires all rows below. Each test MUST exercise human and JSON behavior where applicable, assert server-returned URLs, and verify no settled authority boundary is crossed.

| Contract | Required acceptance evidence |
| --- | --- |
| Domain/Project/identity | Canonical-path equivalence, Project creation/move/tombstone, Service name NFC/case rules, 26-character random IDs, non-reuse, slug normalization/collision/rename redirects, `404` versus `410`. |
| Artifact Publish | Single-file/directory entry precedence, `.obs.json`, MIME/nosniff, byte identity, dotfiles, relative assets, no fallback/listing, warning/block split, strict create/replace, immutable/stable URLs. |
| Filesystem safety/import | Every rejected inode/path form, no-follow traversal, source-race detection, explicit ordered import, no discovery/live link/path persistence, advisory duplicates, per-entry atomic partial retry. |
| Service | Strict registration/update, one primary Target, atomic primary replacement, direct Open, alternatives only in details, removal without process effect, teardown success/failure/timeout/concurrent-change safeguards. |
| Routes/API | All six namespaces, absolute redirect/canonicalization/query rules, hostile encoding/traversal corpus, arbitrary bundle segments, API v1 exactness/no unversioned redirect, cache headers, URL fields never client-derived. |
| CLI | Complete B help tree including recovery extensions, project resolution, human stdout/stderr, one-value JSON envelopes, exits 0/2–10, non-TTY confirmation, idempotent replay/conflict/unknown commit retry, ordered partials, daemon unavailable/second-daemon behavior. |
| Ledger/browser | Approved Project-first hierarchy and orders/filters/search; all Artifact/Service states; direct Service Open; separate details; no embedded preview; no-JavaScript core flow. Test Chromium, keyboard-only, screen-reader semantics, reduced motion, contrast, 200% zoom/reflow, and mobile cards/navigation on phone viewport and real tailnet phone. |
| Artifact lifecycle | All retention modes/deadline transitions, recovery/restore/early purge, `410`, Revision age+count rule, pressure order, reserve/limits, pin/current protection, preview, per-candidate failures, audit completeness. |
| Service liveness | Host-vantage method/status semantics, startup/scheduled/immediate/manual probes, exact state ages, version invalidation/late discard, all-offline grace/reset/keep/pin/unpin, fresh cleanup confirmation, shared bounds/coalescing/partial isolation. |
| Persistence | SQLite constraints/policy, exact layout/permissions, manifest digest, same-filesystem enforcement, Publish and cleanup visibility ordering, intent recovery/quarantine, backup leases and complete backup, migration gates/candidates/authority cutover. |
| Diagnostics/recovery | Every profile/check/state/category; health versus ok; redaction; availability gates; partial order; preview fingerprint expiry/change; apply/resume idempotency; reconcile limits; candidate loss reports; activate/restore/discard/rollback locks, confirmations, audit, and crash boundaries. |
| Security/network | Loopback-only rejection, Host/same-origin browser mutation checks, explicit Tailscale grant from host and remote intended client, denied ungranted/LAN/public client, no Funnel, Serve conflict/preservation/ownership, no Service proxy, direct Target authorization independence. |
| Deployment | XDG precedence/invalid paths/no reload, embedded assets, release contents/activation, setup/linger/unit, startup/readiness/degraded modes, SIGTERM/restart limits, update/migration/rollback/logging/Tailscale outage. |

The six stack spike gates are mandatory:

1. **Adversarial walker races:** symlink swaps, renamed parents/children, mount crossings, unusual inode types, and concurrent source changes prove no escape, follow, unsafe copy, descriptor leak, or overwrite.
2. **Crash and fault injection:** fail after every transaction, sync, rename, WAL, migration, backup, cleanup, activation, and shutdown boundary; inject partial writes, I/O/full-disk/busy/checkpoint failures; restart reaches only a valid settled state or specified gate.
3. **Every-connection SQLite policy:** every connection creation/reuse/failure/replacement proves pinned engine, application/schema identity, foreign keys, WAL, `synchronous=FULL`, and bounded busy behavior before use.
4. **Tokio thread policy and load:** prove four-worker/four-blocking limits and bounded queues under reads, streaming, Publish, cleanup, probes, cancellation, and shutdown; record CPU/RSS/thread/latency evidence.
5. **systemd/package/Serve integration:** test real user-manager setup, linger, lifecycle, SIGTERM/SIGKILL recovery, restart limits, journald, activation, health, migration backup, compatible/incompatible rollback, unrelated Serve preservation, root conflict, tailnet outage, and deployed Tailscale behavior.
6. **Target ABI/static options:** test the released glibc binary on declared baseline and desktop, inspect linkage, and prove bundled SQLite and rustls. Other targets remain unsupported pending equivalent matrices.

A failed acceptance test is implementation work under this specification. It becomes a new planning decision only when satisfying the test requires changing a fixed contract.

## 18. Deferred work and unsupported configurations

Explicitly deferred:

- production implementation, installation, deployment, and current `AGENTS.md` onboarding;
- fixing Sideshow loopback/public-origin behavior;
- producer-specific conversion/snapshot adapters;
- application-specific Service release checks beyond Observatory's generic contract;
- multi-user authorization, roles, and separate application authentication;
- multi-host replication/distributed storage;
- old-origin redirects after deployment migration; and
- new incompatible API versions.

Unsupported:

- public internet/Funnel and LAN-only/non-tailnet clients;
- backend listeners outside loopback;
- remote/network or cross-mount storage where the atomic protocol applies;
- filesystem discovery, source links, project-root crawling, live imports, or move/delete-source import;
- Service proxying, automatic Target fallback, automatic external runtime action, or Service health claims;
- generic file browsing, directory listing, SPA fallback, and embedded catalogue previews;
- unmodified Sideshow 0.7.0 as a canonical Service;
- musl, non-x86-64, other Linux distributions without the full declared matrix, macOS, and Windows;
- containers, Arch/Debian/RPM packages, Node/Bun/npm runtime, external database/search, or filesystem-only authority; and
- storage rebuild that silently treats manifests/filesystem bytes as authority.

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
- Never proxy, embed, absorb, copy, or manage Service state through Observatory. Service Open uses the server-returned direct primary Target; `service remove` changes only the catalogue, and explicit `service teardown` is the sole optional external runtime action.
```

## 20. Decision traceability

| Decision | Specification coverage | Durable rationale/evidence |
| --- | --- | --- |
| [#2 Artifact contract](https://github.com/Whamp/observatory/issues/2) | §§3, 5, 7, 10 | `CONTEXT.md`; Artifact resolution |
| [#3 Service contract](https://github.com/Whamp/observatory/issues/3) | §§3–4, 6, 11 | Service resolution |
| [#4 Tailscale Service validation](https://github.com/Whamp/observatory/issues/4) | §§6, 14, 17–18 | [Serve report](docs/research/2026-07-09-tailscale-serve-live-services.md); [probe transcript](docs/research/evidence/2026-07-09-tailscale-serve-probe.md) |
| [#5 canonical address/trust](https://github.com/Whamp/observatory/issues/5) | §§1, 7, 14, 16 | Tailscale primary-source links in issue resolution and Serve report |
| [#6 routes/namespaces/slugs](https://github.com/Whamp/observatory/issues/6) | §§4, 7 | Route resolution |
| [#7 Artifact retention](https://github.com/Whamp/observatory/issues/7) | §§10, 12–13, 16–17 | Retention resolution; [persistence note](docs/research/2026-07-09-observatory-persistence-architecture.md) |
| [#8 Service liveness](https://github.com/Whamp/observatory/issues/8) | §§6, 9, 11, 16–17 | Liveness resolution |
| [#9 CLI contract](https://github.com/Whamp/observatory/issues/9) | §§8, 13, 16–17, 19 | [approved B CLI artifact](docs/prototypes/observatory-cli-contract.html) at [committed decision artifact](https://github.com/Whamp/observatory/blob/1bf95af1d41e98450b09a4eb1e81846915459e9c/docs/prototypes/observatory-cli-contract.html?variant=B) |
| [#10 index/navigation](https://github.com/Whamp/observatory/issues/10) | §§9, 15, 17 | [approved B index artifact](docs/prototypes/observatory-index-navigation.html) at [commit `6362d84`](https://github.com/Whamp/observatory/commit/6362d84c2f8508234968e7deb2c91a8cc090c0bd) |
| [#11 persistence/indexing](https://github.com/Whamp/observatory/issues/11) | §§12–13, 15–17 | [persistence architecture](docs/research/2026-07-09-observatory-persistence-architecture.md) |
| [#12 stack/packaging/supervision](https://github.com/Whamp/observatory/issues/12) | §§15–18 | [stack decision note](docs/research/2026-07-09-observatory-implementation-stack.md) at [commit `f4263ec`](https://github.com/Whamp/observatory/commit/f4263ecd3782c73f002799b36da24df498ad074c) |
| [#15 import](https://github.com/Whamp/observatory/issues/15) | §§5, 8, 12, 17–18 | Import resolution; persistence architecture |
| [#16 diagnostics/recovery](https://github.com/Whamp/observatory/issues/16) | §§8, 12–13, 16–17 | [diagnostics/recovery note](docs/research/2026-07-09-observatory-storage-diagnostics-recovery.md) at [commit `4ba5504`](https://github.com/Whamp/observatory/commit/4ba55043feb77b58decf906900c96ee0755759eb) |

The [map](https://github.com/Whamp/observatory/issues/1), [`CONTEXT.md`](CONTEXT.md), all linked closed resolutions, all four research notes, the probe transcript, and both committed prototypes are the complete durable source set used for this synthesis. No settled decision was reopened.
