# Observatory public control plane

Date: 2026-07-09

Ticket: [Define the versioned local API and complete operation surface](https://github.com/Whamp/observatory/issues/17)

## 1. Decision
Observatory exposes one cohesive, versioned control plane under `/api/v1`. The CLI, browser UI, and scheduled workers are clients of the same application service and schemas; none may open SQLite, inspect storage as authority, construct canonical public URLs, or independently implement lifecycle rules. The local setup and service-management leaves use the same output envelope but are bootstrap authority outside the daemon API.
This resolution preserves all settled decisions from `SPEC.md`, issues #2, #3, #6–#10, #15, and #16:
- explicit Project, Artifact, Revision, Service, and Target domains;
- opaque, never-reused identities and durable tombstones;
- server-returned canonical URLs;
- Project-led UI;
- daemon and SQLite authority;
- one-envelope JSON;
- ordered per-item results;
- retention, recovery, persistence, and security boundaries; and
- Service runtime independence.
The API is not generic CRUD. Its operations name the domain transitions: Publish, replace, restore, pin, teardown, refresh, keep, cleanup, recovery planning, activation, and explicit Project move.
---
## 2. Common HTTP contract
### 2.1 Media, caching, and envelopes
All `/api/v1` requests and responses use UTF-8 JSON:
```http
Content-Type: application/json
Accept: application/json
Cache-Control: no-store
```
A successful or trustworthy result uses:
```json
{
  "schemaVersion": 1,
  "ok": true,
  "result": {}
}
```
A command-level inability uses:
```json
{
  "schemaVersion": 1,
  "ok": false,
  "error": {
    "code": "changed_record",
    "message": "the Service changed after it was read",
    "retryable": false,
    "details": {
      "expectedRecordVersion": 7,
      "actualRecordVersion": 8
    }
  }
}
```
`ok` answers only whether Observatory returned a trustworthy result for the requested operation. It does not mean:
- every batch item succeeded;
- storage is healthy;
- a Service is reachable; or
- a teardown process exited successfully.
The API deliberately retains Observatory’s settled envelope rather than adopting RFC 9457 Problem Details as a second incompatible error model. RFC 9457 remains useful rationale for stable machine-readable fields. [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457)
Unknown JSON fields are ignored on input only where explicitly marked extensible. Unknown enum values are never silently coerced.
### 2.2 Time, duration, byte, and ID types
| Type | Representation |
|---|---|
| Instant | RFC 3339 UTC string with `Z`, millisecond precision |
| API duration | integer milliseconds, `0` only where explicitly allowed |
| CLI duration | positive compound duration such as `30s`, `15m`, `7d`; converted exactly to milliseconds |
| Byte count | unsigned JSON integer |
| Record version | positive unsigned JSON integer |
| Resource ID | exactly 26 lowercase Crockford-base32 characters |
| Resource key | `<slug>~<id>` |
| Digest | lowercase `<algorithm>:<hex>`, initially `sha256:<64 hex>` |
JSON integers must remain within the exact integer range accepted by the server. Negative byte counts, durations, versions, limits, and indexes are invalid.
### 2.3 Shared resource reference
```json
{
  "id": "6r3t8w2y5b9c4d7f0g1h3j5k7m",
  "key": "agent-cli-contract~6r3t8w2y5b9c4d7f0g1h3j5k7m",
  "recordVersion": 7,
  "apiUrl": "https://desktop.greyhound-chinstrap.ts.net/api/v1/artifacts/6r3t8w2y5b9c4d7f0g1h3j5k7m",
  "detailUrl": "https://desktop.greyhound-chinstrap.ts.net/ui/projects/observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p/artifacts/agent-cli-contract~6r3t8w2y5b9c4d7f0g1h3j5k7m/"
}
```
Every create, get, update, transition, and per-item success returns:
- opaque IDs;
- the current display key where the resource has one;
- `recordVersion`;
- `apiUrl`;
- all applicable canonical browser URLs.
Clients must not construct URLs from IDs, keys, titles, names, paths, slugs, or the configured origin.
### 2.4 Locator and resolution rules
API path parameters use opaque IDs, not keys or names, except Target names:
```text
/api/v1/artifacts/{artifactId}
/api/v1/services/{serviceId}
/api/v1/projects/{projectId}
/api/v1/revisions/{revisionId}
/api/v1/services/{serviceId}/targets/{targetName}
```
A key supplied to the CLI is parsed only to obtain its ID. Its slug is not an identity claim. A stale but syntactically valid slug resolves to the same ID and the response returns the current key and URLs.
CLI selectors are:
- Project: Project ID, Project key, or explicit filesystem path through `project resolve`;
- Artifact: Artifact ID or key;
- Revision: Revision ID;
- Service: Service ID/key, or NFC Service name scoped to the selected Project;
- Target: exact NFC Target name scoped to a Service.
Ambiguous or unscoped Service names are rejected rather than guessed.
Resolution outcomes are exact:
| Condition | HTTP |
|---|---:|
| Live issued ID | resource result |
| Tombstoned issued ID | `410 Gone` |
| Unknown/never-issued ID | `404 Not Found` |
| Wrong resource type | `404 Not Found` |
| Malformed ID or key | `422 Unprocessable Content` |
| Stale key slug in API request body | accepted by ID, response returns current key |
| Stale browser-route slug | settled absolute `308` canonical redirect |
API routes never redirect to repair IDs, keys, slashes, or API versions.
### 2.5 Record versions, ETags, and preconditions
Every mutable singleton representation has:
```http
ETag: "rv-7"
```
and:
```json
"recordVersion": 7
```
The ETag is a strong opaque validator of the complete mutation-relevant record. Mutations of an existing resource require:
```http
If-Match: "rv-7"
```
This applies to `PATCH`, `DELETE`, and action POSTs such as replace, pin, restore, keep, teardown, Target promotion, and recovery apply.
| Condition | Result |
|---|---|
| Missing required `If-Match` | `428 Precondition Required`, `precondition_required` |
| ETag does not match | `412 Precondition Failed`, `changed_record` |
| State conflicts despite matching version | `409 Conflict`, domain-specific conflict |
| Successful mutation | new `ETag`, new `recordVersion` |
Collection creation does not require `If-Match`. Strict creation conflicts use `409`, not `412`.
This follows HTTP conditional request semantics: failed `If-Match` is `412`; a server requiring a precondition may use `428`. [RFC 9110 §13](https://www.rfc-editor.org/rfc/rfc9110#section-13), [RFC 6585 §3](https://www.rfc-editor.org/rfc/rfc6585#section-3)
### 2.6 Idempotency-Key
Every API mutation requires:
```http
Idempotency-Key: "issue-17-artifact-001"
```
Browser code generates a fresh cryptographically random key for each confirmed submission. Non-TTY and automation CLI mutations require an explicit key. An interactive TTY mutation may generate a key, but it must print the key before dispatch. This required transport intentionally sharpens the earlier SHOULD so future onboarding can safely say “always use an idempotency key.”
Exact key syntax:
- 8–200 visible ASCII characters;
- no whitespace, control characters, quotes, or backslashes;
- compared byte-for-byte and scoped to the Observatory deployment;
- treated as sensitive operational metadata in logs.
The canonical request fingerprint is SHA-256 over:
1. API version;
2. uppercase HTTP method;
3. exact canonical API route;
4. canonical RFC 8785 JSON body;
5. normalized resource IDs and resolved canonical Project/source paths;
6. `If-Match` value where required; and
7. operation-semantic options such as pressure, timeout, retention, and confirmation-plan ID.
Transport-only fields such as `Accept`, user agent, client timeout, and tracing headers are excluded.
For host source operations, the first accepted attempt additionally binds its durable intent to the observed source snapshot. A retry resumes or replays that intent; it never silently republishes later source bytes under the old key.
Behavior:
| Situation | Result |
|---|---|
| New key | execute or durably start operation |
| Same key and fingerprint, terminal | replay exact semantic result |
| Same key and fingerprint, nonterminal | resume or report current operation |
| Same key, different fingerprint | `409 idempotency_conflict` |
| Concurrent duplicate still running | `409 idempotency_in_progress`, retryable, with `Retry-After` |
| Pre-dispatch validation failure | no durable key consumption |
| Effect committed, response lost | identical retry returns committed result |
Replays include:
```http
Idempotency-Replayed: true
```
and preserve the original canonical URLs, operation ID, status semantics, and resulting ETag.
This follows the IETF Idempotency-Key draft’s key/fingerprint/replay model. [IETF Idempotency-Key draft §2](https://datatracker.ietf.org/doc/html/draft-ietf-httpapi-idempotency-key-header#section-2)
### 2.7 Timeout and unknown commit
The CLI’s `--timeout` is a client wait deadline, not authority to cancel a durable operation.
When the deadline expires after request dispatch, the CLI must:
- close its wait;
- leave stdout empty;
- emit `ok:false`, `client_timeout` on stderr;
- exit `5`;
- state that commit is unknown; and
- instruct an identical retry with the same key.
```json
{
  "schemaVersion": 1,
  "ok": false,
  "error": {
    "code": "client_timeout",
    "message": "deadline reached after dispatch; commit state is unknown",
    "retryable": true,
    "details": {
      "idempotencyKey": "report-44",
      "retry": "repeat the identical request with the same key"
    }
  }
}
```
A client disconnect does not roll back or cancel a durable server operation. Observatory does not manufacture a `504` response after losing the connection.
A known operation timeout, such as Teardown Action timeout, is not unknown commit: it returns a trustworthy teardown outcome, preserves the Service, and maps to exit `9`.
### 2.8 Pagination, filtering, and ordering
All unbounded collection endpoints accept:
```text
limit=50            # 1..200
after=<opaque>
order=<enum>
direction=asc|desc
```
Responses use:
```json
{
  "items": [],
  "page": {
    "limit": 50,
    "nextCursor": "opaque-or-null",
    "hasMore": false
  }
}
```
When another page exists, the response also includes an absolute RFC 8288 link:
```http
Link: <https://.../api/v1/artifacts?limit=50&after=...>; rel="next"
```
Cursors are opaque, integrity-protected, bind the exact endpoint/filter/order tuple, and expire after 15 minutes. Clients must not decode them.
- malformed or filter-mismatched cursor: `422 invalid_cursor`;
- expired cursor: `409 cursor_expired`;
- omitted cursor: start a new traversal.
Ordering always has an opaque ID as its final ascending tie-breaker.
Common filters:
| Endpoint | Filters | Orders |
|---|---|---|
| Projects | `state=live|gone|all`, `query` | `title`, `recent` |
| Artifacts | `projectId`, `state`, `retentionMode`, `query` | `recent`, `title`, `attention` |
| Services | `projectId`, `reachability`, `pinned`, `query` | `recent`, `title`, `attention` |
| Revisions | `availability=current|superseded|gone|all` | `published`, `superseded` |
| Ledger | `projectId`, `kind=all|artifact|service`, `query` | `recent`, `title`, `attention` |
| Audit | `resourceType`, `resourceId`, `cause`, `actor`, `since`, `until` | `timestamp` |
`recent` means latest successful Publish/replacement for Artifacts and latest observation or register/update/keep activity for Services, matching the UI contract.
Search is literal Unicode-aware case-folded substring matching over the settled visible fields. It is not relevance-ranked full-text search.
Pagination links follow the registered Web Linking model. [RFC 8288](https://www.rfc-editor.org/rfc/rfc8288)
---
## 3. Shared schemas
### 3.1 Project
```json
{
  "kind": "project",
  "id": "4m7k2x9q1v6c8d3f5g0h2j4n6p",
  "key": "observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p",
  "recordVersion": 3,
  "state": "live",
  "title": "Observatory",
  "slug": "observatory",
  "canonicalDirectory": "/home/will/projects/observatory",
  "createdAt": "2026-07-09T17:40:00.000Z",
  "updatedAt": "2026-07-09T18:00:00.000Z",
  "apiUrl": "https://desktop.greyhound-chinstrap.ts.net/api/v1/projects/4m7k2x9q1v6c8d3f5g0h2j4n6p",
  "detailUrl": "https://desktop.greyhound-chinstrap.ts.net/ui/projects/observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p/"
}
```
Gone Projects additionally include `terminalState`, `tombstonedAt`, and `cause`, while retaining no redirect to another Project.
### 3.2 Retention
```json
{
  "mode": "default",
  "ttlMs": 2592000000,
  "expiresAt": "2026-08-08T17:40:00.000Z",
  "pinReason": null,
  "recoveryUntil": null
}
```
Modes are `default`, `ttl`, and `pinned`. A recoverable Artifact retains its previous mode metadata and separately reports `recoveryUntil`.
### 3.3 Artifact
```json
{
  "kind": "artifact",
  "id": "6r3t8w2y5b9c4d7f0g1h3j5k7m",
  "key": "agent-cli-contract~6r3t8w2y5b9c4d7f0g1h3j5k7m",
  "recordVersion": 7,
  "state": "live",
  "title": "Agent CLI contract",
  "description": "Public control-plane contract",
  "slug": "agent-cli-contract",
  "project": {
    "id": "4m7k2x9q1v6c8d3f5g0h2j4n6p",
    "key": "observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p"
  },
  "currentRevisionId": "7s4v9x3z6c0d5f8g1h2j4k6m8n",
  "retention": {
    "mode": "default",
    "ttlMs": 2592000000,
    "expiresAt": "2026-08-08T17:40:00.000Z",
    "pinReason": null,
    "recoveryUntil": null
  },
  "files": 4,
  "logicalBytes": 183442,
  "revisionCount": 3,
  "publishedAt": "2026-07-09T17:40:00.000Z",
  "updatedAt": "2026-07-09T18:10:00.000Z",
  "apiUrl": "https://desktop.greyhound-chinstrap.ts.net/api/v1/artifacts/6r3t8w2y5b9c4d7f0g1h3j5k7m",
  "openUrl": "https://desktop.greyhound-chinstrap.ts.net/artifacts/agent-cli-contract~6r3t8w2y5b9c4d7f0g1h3j5k7m/",
  "detailUrl": "https://desktop.greyhound-chinstrap.ts.net/ui/projects/observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p/artifacts/agent-cli-contract~6r3t8w2y5b9c4d7f0g1h3j5k7m/"
}
```
### 3.4 Revision
```json
{
  "kind": "revision",
  "id": "7s4v9x3z6c0d5f8g1h2j4k6m8n",
  "artifactId": "6r3t8w2y5b9c4d7f0g1h3j5k7m",
  "state": "current",
  "entryPath": "index.html",
  "entryMediaType": "text/html",
  "files": 4,
  "logicalBytes": 183442,
  "manifestDigest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "publishedAt": "2026-07-09T17:40:00.000Z",
  "apiUrl": "https://desktop.greyhound-chinstrap.ts.net/api/v1/revisions/7s4v9x3z6c0d5f8g1h2j4k6m8n",
  "openUrl": "https://desktop.greyhound-chinstrap.ts.net/revisions/7s4v9x3z6c0d5f8g1h2j4k6m8n/"
}
```
Revision states are `current`, `superseded`, `unavailable`, and `gone`.
### 3.5 Service, Target, and observation
```json
{
  "kind": "service",
  "id": "8t5w0x4z7c1d6f9g2h3j5k7m9n",
  "key": "pi-annotate~8t5w0x4z7c1d6f9g2h3j5k7m9n",
  "recordVersion": 8,
  "state": "live",
  "name": "pi-annotate",
  "label": "Current annotation session",
  "description": "",
  "slug": "pi-annotate",
  "project": {
    "id": "4m7k2x9q1v6c8d3f5g0h2j4n6p",
    "key": "observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p"
  },
  "pinned": false,
  "pinReason": null,
  "expiryDeadline": null,
  "primaryTargetName": "tailnet",
  "reachability": "online",
  "teardown": {
    "available": true,
    "timeoutMs": 30000
  },
  "targets": [
    {
      "name": "tailnet",
      "label": "Tailnet",
      "url": "https://desktop.greyhound-chinstrap.ts.net:8443/session/current",
      "primary": true,
      "targetVersion": 2,
      "reachability": "online",
      "observation": {
        "result": "response",
        "observedAt": "2026-07-09T17:44:02.000Z",
        "durationMs": 42,
        "httpStatus": 302,
        "failureCategory": null,
        "hostVantage": "backend"
      }
    }
  ],
  "apiUrl": "https://desktop.greyhound-chinstrap.ts.net/api/v1/services/8t5w0x4z7c1d6f9g2h3j5k7m9n",
  "detailUrl": "https://desktop.greyhound-chinstrap.ts.net/ui/projects/observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p/services/pi-annotate~8t5w0x4z7c1d6f9g2h3j5k7m9n/",
  "primaryTargetUrl": "https://desktop.greyhound-chinstrap.ts.net:8443/session/current"
}
```
The Teardown Action’s executable and argv are accepted on register/update but are not returned by ordinary representations. Details expose only availability and configured timeout. Recovery and diagnostics redact argv.
### 3.6 Per-item outcome
```json
{
  "index": 1,
  "label": "live-dashboard",
  "status": "failed",
  "result": null,
  "error": {
    "code": "source_changed",
    "message": "source changed while it was copied",
    "retryable": true,
    "details": {}
  }
}
```
`status` is operation-specific but always one of the documented terminal values. Exactly one of `result` or `error` is non-null.
---
## 4. Exact `/api/v1` endpoint inventory
### 4.1 Projects and Project-led ledger
| Method | Path | Operation |
|---|---|---|
| `GET` | `/api/v1/projects` | List Projects |
| `POST` | `/api/v1/projects` | Strictly register one canonical Project |
| `GET` | `/api/v1/projects/resolve?path=…` | Canonicalize path and report existing identity, without allocation |
| `GET` | `/api/v1/projects/ledger` | All-Projects combined ledger |
| `GET` | `/api/v1/projects/{projectId}` | Show live Project |
| `PATCH` | `/api/v1/projects/{projectId}` | Update title or slug only |
| `GET` | `/api/v1/projects/{projectId}/ledger` | Project-scoped combined ledger |
| `POST` | `/api/v1/projects/{projectId}/move` | Explicit atomic Project move |
| `DELETE` | `/api/v1/projects/{projectId}` | Tombstone an empty live Project |
No `/entries` endpoint exists.
### 4.2 Artifacts and Revisions
| Method | Path | Operation |
|---|---|---|
| `GET` | `/api/v1/artifacts` | List Artifacts |
| `POST` | `/api/v1/artifacts` | Strict Publish |
| `POST` | `/api/v1/artifact-imports` | Ordered explicit import batch |
| `GET` | `/api/v1/artifacts/{artifactId}` | Show Artifact |
| `PATCH` | `/api/v1/artifacts/{artifactId}` | Update title, description, or slug |
| `DELETE` | `/api/v1/artifacts/{artifactId}` | Enter recovery window |
| `POST` | `/api/v1/artifacts/{artifactId}/replace` | Publish and select immutable Revision |
| `POST` | `/api/v1/artifacts/{artifactId}/restore` | Restore recoverable Artifact |
| `POST` | `/api/v1/artifacts/{artifactId}/pin` | Pin |
| `POST` | `/api/v1/artifacts/{artifactId}/unpin` | Unpin with TTL/default |
| `POST` | `/api/v1/artifacts/{artifactId}/purge-plans` | Preview exact early permanent purge |
| `POST` | `/api/v1/artifact-purge-plans/{planId}/apply` | Apply early purge |
| `GET` | `/api/v1/artifacts/{artifactId}/revisions` | Ordered Revision history |
| `GET` | `/api/v1/revisions/{revisionId}` | Show Revision metadata |
Byte serving remains exclusively under `/artifacts/…` and `/revisions/…`, not the API.
### 4.3 Services and Targets
| Method | Path | Operation |
|---|---|---|
| `GET` | `/api/v1/services` | List Services |
| `POST` | `/api/v1/services` | Strict Service registration |
| `GET` | `/api/v1/services/{serviceId}` | Show Service and all Targets |
| `PATCH` | `/api/v1/services/{serviceId}` | Update presentation, Targets as one atomic set, or Teardown Action |
| `DELETE` | `/api/v1/services/{serviceId}` | Remove catalogue metadata only |
| `POST` | `/api/v1/services/{serviceId}/teardown` | Run explicit action, remove only unchanged record on exit 0 |
| `POST` | `/api/v1/services/{serviceId}/refresh` | Refresh every Target |
| `POST` | `/api/v1/services/refresh` | Refresh all Services |
| `POST` | `/api/v1/services/{serviceId}/pin` | Pin |
| `POST` | `/api/v1/services/{serviceId}/unpin` | Unpin |
| `POST` | `/api/v1/services/{serviceId}/keep` | Restart all-offline grace only |
| `GET` | `/api/v1/services/{serviceId}/targets` | List Targets |
| `POST` | `/api/v1/services/{serviceId}/targets` | Add Target |
| `GET` | `/api/v1/services/{serviceId}/targets/{targetName}` | Show Target |
| `PATCH` | `/api/v1/services/{serviceId}/targets/{targetName}` | Update URL or label |
| `DELETE` | `/api/v1/services/{serviceId}/targets/{targetName}` | Remove Target, atomically naming replacement when primary |
| `POST` | `/api/v1/services/{serviceId}/targets/{targetName}/promote` | Make primary |
| `POST` | `/api/v1/services/{serviceId}/targets/{targetName}/refresh` | Refresh one Target |
Target names are NFC-normalized, nonempty after trimming, control-free, case-sensitive UTF-8. A Target name in a route is encoded as one UTF-8 segment; encoded slash, backslash, NUL, malformed escapes, and traversal forms are rejected.
### 4.4 Cleanup
| Method | Path | Operation |
|---|---|---|
| `GET` | `/api/v1/cleanup/preview?pressure=false` | Read-only candidate preview |
| `POST` | `/api/v1/cleanup/runs` | Execute eligible cleanup |
| `GET` | `/api/v1/cleanup/runs/{operationId}` | Show durable cleanup result/state |
### 4.5 Diagnostics, recovery, backup, configuration, audit, and health
| Method | Path | Operation |
|---|---|---|
| `GET` | `/api/v1/system/health` | Lightweight post-reconciliation readiness/health |
| `GET` | `/api/v1/system/status` | Fast storage/system status |
| `POST` | `/api/v1/system/diagnostics` | Normal or deep diagnostic profile |
| `POST` | `/api/v1/system/recovery/plans` | Durable read-only recovery preview |
| `GET` | `/api/v1/system/recovery/plans/{planId}` | Show plan |
| `POST` | `/api/v1/system/recovery/plans/{planId}/apply` | Apply exact plan |
| `POST` | `/api/v1/system/recovery/resume` | Resume matching nonterminal intent |
| `GET` | `/api/v1/system/recovery/operations/{operationId}` | Show operation |
| `POST` | `/api/v1/system/backups` | Create complete backup |
| `POST` | `/api/v1/system/backups/verify` | Verify named backup |
| `GET` | `/api/v1/system/configuration` | Redacted effective configuration and restart-required status |
| `POST` | `/api/v1/system/configuration/validate` | Validate proposed TOML content without installing or activating it |
| `GET` | `/api/v1/system/audit` | Paginated audit events |
`POST /system/diagnostics` is used because deep diagnostics acquire a maintenance gate and perform the settled disposable filesystem capability probe, even though they do not mutate catalogue authority.
No remote API installs, updates, removes, or uninstalls binaries, units, stable command links, or Tailscale handlers. [Issue #19](https://github.com/Whamp/observatory/issues/19) owns the local bootstrap implementation internals, but it cannot change the setup or service leaf meanings fixed here.
---
## 5. Operation requests and exact semantics
### 5.1 Project resolve and register
Resolve:
```http
GET /api/v1/projects/resolve?path=%2Fhome%2Fwill%2Fprojects%2Fobservatory
```
```json
{
  "schemaVersion": 1,
  "ok": true,
  "result": {
    "inputPath": "/home/will/projects/observatory",
    "canonicalDirectory": "/home/will/projects/observatory",
    "status": "registered",
    "project": {
      "id": "4m7k2x9q1v6c8d3f5g0h2j4n6p",
      "key": "observatory~4m7k2x9q1v6c8d3f5g0h2j4n6p"
    }
  }
}
```
`status` is `registered`, `unregistered`, or `gone`.
Resolution:
- is read-only;
- is performed by the daemon;
- requires the path to exist and be a directory;
- follows the platform’s canonical path resolution once;
- does not allocate an ID;
- reports `gone` when that canonical directory has a terminal Project identity.
Register:
```json
{
  "path": "/home/will/projects/observatory",
  "title": "Observatory",
  "slug": "observatory"
}
```
Registration:
1. canonicalizes the directory;
2. rejects nonexistent/non-directory/inaccessible paths;
3. conflicts if a live Project already owns it;
4. returns `410 project_gone` if it is permanently associated with a tombstoned Project;
5. allocates a random Project ID and stable creation slug; and
6. commits before it can be selected by Service registration.
### 5.2 Project move
```http
POST /api/v1/projects/{oldProjectId}/move
If-Match: "rv-3"
Idempotency-Key: project-move-20260709
```
```json
{
  "newPath": "/home/will/projects/observatory-renamed",
  "title": "Observatory",
  "slug": "observatory",
  "services": [
    {
      "oldServiceId": "8t5w0x4z7c1d6f9g2h3j5k7m9n",
      "expectedRecordVersion": 8
    }
  ]
}
```
The move is one explicit, atomic catalogue transition:
1. canonicalize `newPath`;
2. require it to be unregistered and not historically gone;
3. require the request to enumerate every live Service in the old Project exactly once;
4. require matching Service record versions;
5. allocate a new Project ID and new IDs for all re-registered Services;
6. copy each Service’s settled configuration, Targets, primary choice, pin metadata, and Teardown Action into the new Project;
7. reset every new Target observation to `unknown` and queue probes;
8. tombstone every old Service ID;
9. tombstone the old Project ID with cause `moved`;
10. leave all Artifacts associated with the old tombstoned Project;
11. leave Artifact serving URLs unchanged; and
12. return ordered old-to-new Project and Service mappings.
It does not redirect old IDs, infer filesystem movement, reassociate Artifacts, execute teardown, or alter external processes.
Any missing Service, extra Service, version change, path conflict, validation failure, or capacity/authority gate aborts the entire transition.
### 5.3 Project tombstone
`DELETE /projects/{id}` requires:
- matching `If-Match`;
- confirmation at the CLI/UI layer;
- zero live Services;
- no nonterminal Project-scoped operation.
Artifacts may remain associated and continue their own lifecycle. They are not deleted or reassociated. The Project becomes permanently `410 Gone`.
### 5.4 Publish and replace
Publish request:
```json
{
  "source": {
    "path": "/home/will/projects/observatory/report",
    "callerWorkingDirectory": "/home/will/projects/observatory"
  },
  "projectId": "4m7k2x9q1v6c8d3f5g0h2j4n6p",
  "entry": "index.html",
  "title": "Control plane",
  "description": "Issue 17 resolution",
  "slug": "control-plane",
  "retention": {
    "mode": "ttl",
    "ttlMs": 1209600000,
    "pinReason": null
  }
}
```
The client converts a relative source selection to an absolute lexical selection using its invocation working directory. The daemon performs all canonicalization, descriptor-relative opening, validation, copying, and source-race checks. Absolute source paths are request data only and never enter remote results, durable provenance, logs, or audit details. More broadly, remote diagnostics, audit events, and errors never include absolute paths. The authorized Project representation is the sole exception: `canonicalDirectory` remains visible because it is the Project identity field.
Replace uses the same publish fields at:
```text
POST /artifacts/{artifactId}/replace
```
and requires `If-Match`.
### 5.5 Import
```json
{
  "projectId": "4m7k2x9q1v6c8d3f5g0h2j4n6p",
  "defaults": {
    "retention": {"mode": "default"},
    "entry": null,
    "title": null,
    "description": null,
    "slug": null
  },
  "items": [
    {
      "source": {
        "path": "/tmp/auth-flow.html",
        "callerWorkingDirectory": "/home/will/projects/observatory"
      },
      "label": "auth-flow",
      "options": null
    },
    {
      "source": {
        "path": "/tmp/dashboard",
        "callerWorkingDirectory": "/home/will/projects/observatory"
      },
      "label": "dashboard",
      "options": {
        "projectId": "4m7k2x9q1v6c8d3f5g0h2j4n6p",
        "retention": {
          "mode": "pinned",
          "pinReason": "migration baseline"
        }
      }
    }
  ]
}
```
Per-item options may override only:
- `projectId`;
- `entry`;
- `title`;
- `description`;
- `slug`;
- `retention`.
They cannot change the source path or index.
Bulk idempotency derives a stable per-item key from:
```text
SHA-256("observatory-import-v1" || 0x00 || request-key || 0x00 || decimal-index)
```
Repeated normalized selections in one request fail every repeated occurrence as `duplicate_selection`; no one occurrence is silently chosen.
Result:
```json
{
  "schemaVersion": 1,
  "ok": true,
  "result": {
    "operation": "import",
    "overall": "partial",
    "partial": true,
    "counts": {
      "requested": 2,
      "succeeded": 1,
      "failed": 1,
      "skipped": 0
    },
    "items": [
      {
        "index": 0,
        "label": "auth-flow",
        "status": "committed",
        "result": {
          "artifactId": "2c5f8h1k4n7q0t3w6y9b2d5g8j",
          "revisionId": "3d6g9j2m5p8s1v4x7z0c3f6h9k",
          "openUrl": "https://desktop.greyhound-chinstrap.ts.net/artifacts/auth-flow~2c5f8h1k4n7q0t3w6y9b2d5g8j/",
          "retention": {
            "mode": "default",
            "ttlMs": 2592000000,
            "expiresAt": "2026-08-08T17:40:00.000Z"
          }
        },
        "error": null
      },
      {
        "index": 1,
        "label": "dashboard",
        "status": "failed",
        "result": null,
        "error": {
          "code": "source_changed",
          "message": "source changed while it was copied",
          "retryable": true,
          "details": {}
        }
      }
    ]
  }
}
```
### 5.6 Batch and exit-8 reconciliation
The prototype’s `ok:false` mixed-result example is rejected.
Trustworthy batch, bulk refresh, cleanup, diagnostic, backup verification, and recovery fan-out outcomes always use `ok:true` and go to stdout. Per-item failures belong in `result.items[].error`.
Exact aggregate fields:
| `overall` | Meaning | `partial` |
|---|---|---:|
| `complete` | every requested item reached its successful/unchanged outcome | `false` |
| `partial` | at least one item succeeded and at least one failed/skipped | `true` |
| `failed` | zero items succeeded and at least one trustworthy item failure exists | `false` |
CLI exit behavior:
- `overall=complete`: exit `0`;
- `overall=partial`: exit `8`;
- `overall=failed`: exit `8`, including zero-success batches;
- empty batch or inability to establish trustworthy item outcomes: `ok:false`, stderr, mapped command-level exit.
For diagnostics, `result.partial:true` retains issue #16’s broader meaning: any requested check omitted, errored, or skipped makes the diagnostic partial. Completed unhealthy diagnostics remain exit `10`; inability to execute diagnostics is `ok:false`.
### 5.7 Teardown
Request:
```json
{
  "actionTimeoutMs": 30000
}
```
Default is `30000` ms. Valid range is `1000..300000` ms.
Success requires:
1. action exists;
2. Project canonical directory exists;
3. process launches without a shell;
4. timeout does not expire;
5. process exits `0`;
6. Service record version remains unchanged;
7. Service metadata is then removed and tombstoned.
Launch error, nonzero exit, signal, timeout, missing Project directory, or changed record preserves the Service.
Captured stdout and stderr:
- each limited to 16 KiB after UTF-8 replacement;
- terminal control characters escaped;
- truncation explicitly reported;
- omitted from ordinary logs;
- returned only in the confirmed teardown result/error.
Known timeout:
```json
{
  "schemaVersion": 1,
  "ok": true,
  "result": {
    "operation": "teardown",
    "serviceId": "8t5w0x4z7c1d6f9g2h3j5k7m9n",
    "status": "timed_out",
    "removed": false,
    "actionTimeoutMs": 30000,
    "capturedOutput": {
      "stdout": "",
      "stderr": "session is busy",
      "stdoutTruncated": false,
      "stderrTruncated": false
    }
  }
}
```
The CLI exits `9`. `ok:false` is reserved for inability to attempt or report teardown, not a trustworthy external-action result.
### 5.8 Early Artifact purge
Early purge is a two-step exact plan/apply operation:
1. `POST /artifacts/{id}/purge-plans` with matching `If-Match`;
2. `POST /artifact-purge-plans/{planId}/apply` with `--yes`, matching plan fingerprint, and an idempotency key.
Preview returns exact Artifact/Revision IDs, digests, bytes, current recovery deadline, permanent consequences, preconditions, expiry, and `confirmationRequired:true`.
Apply rejects a live Artifact, changed record, expired plan, active lease, current Revision reference, or broadened scope. Successful purge leaves durable tombstones and audit data and returns reclaimed bytes.
### 5.9 Recovery plans and operations
Recovery plan request:
```json
{
  "operation": "quarantine",
  "selectors": [
    {"kind": "revision", "id": "7s4v9x3z6c0d5f8g1h2j4k6m8n"}
  ]
}
```
Plan representation contains:
- `planId`;
- operation;
- exact selectors;
- identity/digest fingerprints;
- health generation;
- estimated bytes;
- availability impact;
- ambiguity/loss report;
- preconditions;
- rollback point;
- expiry;
- confirmation requirement.
Apply never regenerates or broadens a stale plan. Long-running recovery and backup requests return a durable operation representation. A terminal operation replay returns its terminal result.
---
## 6. HTTP status and CLI exit mapping
| HTTP | Stable codes | CLI exit |
|---:|---|---:|
| `200` | completed read/mutation/replay; trustworthy batch outcome | `0`, `8`, `9`, or `10` according to result |
| `201` | strict resource creation | `0` |
| `202` | durable nonterminal backup/recovery/cleanup operation | `0`; result contains operation URL |
| `204` | not used; envelopes are always returned | — |
| `400` | malformed JSON, malformed query encoding | `2` |
| `404` | `not_found` | `3` |
| `409` | `already_exists`, `idempotency_conflict`, `idempotency_in_progress`, `cursor_expired`, domain-state conflict | `4` |
| `410` | `gone` | `3` |
| `412` | `changed_record` from failed `If-Match` | `4` |
| `413` | request metadata body too large; never used for source bytes | `2` |
| `415` | unsupported media type | `2` |
| `422` | validation, unsafe input, invalid cursor/Target/source/retention | `2`, or `7` for `source_changed` |
| `423` | `maintenance`, authority or resource lock held | `6` |
| `428` | `precondition_required` | `4` |
| `429` | bounded queue/concurrency exhausted | `6` |
| `500` | `internal` | `10` |
| `503` | daemon/storage authority unavailable | `5` or `10` |
| `507` | `capacity`, reserve or configured ceiling | `10` |
`207 Multi-Status` is not used. It is a WebDAV status and would create a second batch contract; Observatory uses HTTP `200` plus its ordered result envelope. [RFC 4918 §11.1](https://www.rfc-editor.org/rfc/rfc4918#section-11.1)
Semantically valid but impossible instructions use `422`; state conflicts use `409`; failed request preconditions use `412`. [RFC 9110 §15](https://www.rfc-editor.org/rfc/rfc9110#section-15)
---
## 7. Complete CLI surface
Global options remain:
```text
--server URL
-p, --project PATH
--json
--timeout DURATION
--idempotency-key KEY
--yes
```
`--yes` is accepted only by leaves that require confirmation.
### 7.1 Artifact
```text
obs artifact publish SOURCE
  [--entry PATH] [--title TEXT] [--description TEXT] [--slug TEXT]
  [--ttl DURATION | --pin] [--reason TEXT]
obs artifact replace ARTIFACT SOURCE
  [--entry PATH] [--title TEXT] [--description TEXT] [--slug TEXT]
  [--ttl DURATION | --pin] [--reason TEXT]
  [--record-version VERSION]
obs artifact import SOURCE...
  [--entry PATH] [--title TEXT] [--description TEXT] [--slug TEXT]
  [--ttl DURATION | --pin] [--reason TEXT]
  [--options FILE]
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
`--options FILE` is a JSON array with one nullable object per positional source. It cannot contain source paths and may override only Project, entry, title, description, slug, and retention.
### 7.2 Service and Target
```text
obs service register NAME
  --target NAME=URL...
  [--primary NAME] [--label TEXT] [--description TEXT] [--slug TEXT]
  [--teardown-arg ARG...] [--action-timeout DURATION]
  [--pin] [--reason TEXT]
obs service update SERVICE
  [--label TEXT] [--description TEXT] [--slug TEXT]
  [--teardown-clear | --teardown-arg ARG...]
  [--action-timeout DURATION]
  [--record-version VERSION]
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
The registered Teardown timeout defaults to 30 seconds. A per-invocation `--action-timeout` overrides it only for that explicit teardown and participates in the idempotency fingerprint.
### 7.3 Project
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
```
`project resolve` never allocates. `project register` is the sole simple allocation leaf. `project move` performs the explicit new-identity/re-registration/tombstone transition.
### 7.4 Cleanup
```text
obs cleanup preview [--pressure]
obs cleanup run --yes [--pressure]
```
### 7.5 System
```text
obs system status
obs system diagnostics [--deep]
obs system recovery preview OPERATION [SELECTOR...]
obs system recovery apply PLAN --yes
obs system recovery resume [OPERATION]
obs system backup create DESTINATION
obs system backup verify BACKUP [--deep]
obs system setup check
obs system setup apply --yes
obs system setup remove --yes
obs system setup uninstall --yes
obs system config show
obs system config validate FILE
obs system service status
obs system service start
obs system service stop --yes
obs system service restart --yes
```
`system config show` reads the daemon’s redacted effective configuration. `system config validate FILE` requires FILE. The CLI opens that local regular file through the safe no-follow input boundary, reads its TOML contents, and sends the content—not the client path—to `POST /api/v1/system/configuration/validate`. There is no stdin mode. The daemon returns ordered parse, schema, and semantic checks and never installs or activates the proposed configuration. The endpoint is non-mutating, requires neither `If-Match` nor `Idempotency-Key`, and a missing daemon returns the normal exit `5`. `setup check` may reuse the same pure parser/schema module locally, but that module has no SQLite, storage, or domain dependency and does not add a local-authority leaf.

The `system setup` and `system service` leaves are local bootstrap authority, not daemon API operations. They still emit the common `schemaVersion`/`ok` result or error envelope and follow the same stdout, stderr, idempotency, timeout, and exit rules. [Issue #19](https://github.com/Whamp/observatory/issues/19) owns their crash-safe implementation internals but cannot change these meanings:

- `setup check`: read-only validation of the executable paths, configuration, loopback bind, storage, user-manager and linger state, generated-unit drift, Tailscale state, canonical origin, and owned Serve root handler;
- `setup apply`: confirmed install or update of Observatory-owned deployment integration, followed by exact loopback health and Serve verification;
- `setup remove`: after ownership checks, remove only deployment integration—the owned Tailscale Serve root handler and generated Observatory user unit—while retaining installed versions, `current`, the stable `obs` command, configuration, and all authoritative data;
- `setup uninstall`: perform `setup remove`, then remove only the verified Observatory-owned stable-command symlink, `current`, version trees, and setup receipts, while retaining configuration and the complete authoritative data root;
- `service status`: read-only unit, installation, and readiness state;
- `service start`: start the installed configured daemon without implicit installation;
- `service stop`: confirmed clean stop; and
- `service restart`: confirmed stop/start followed by exact build/API health verification.

Neither removal leaf purges catalogue or Artifact data, removes foreign or drifted paths, or bypasses ownership checks. No bootstrap download, sudo, `loginctl` mutation, package-manager choice, or installer mechanics are decided here.
### 7.6 Serve and configuration flags
```text
obs serve
  [--listen 127.0.0.1:3773]
  [--canonical-origin URL]
  [--storage PATH]
  [--max-stored-bytes BYTES]
  [--max-live-artifacts COUNT]
  [--teardown-timeout DURATION]
```
---
## 8. Configuration contract
### 8.1 TOML fields
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
Types and meanings:
| Field | Type | Unit/range | Meaning |
|---|---|---|---|
| `server.listen` | string | loopback socket only | backend listener |
| `server.canonical_origin` | absolute HTTPS URL | origin only, trailing `/` | canonical public origin |
| `storage.path` | absolute path string | local supported filesystem | durable root |
| `storage.max_stored_bytes` | unsigned integer | bytes; `0` = unlimited | global Artifact Revision storage ceiling |
| `storage.max_live_artifacts` | unsigned integer | count; `0` = unlimited | live Artifact identity ceiling |
| `service.teardown_timeout_ms` | unsigned integer | `1000..300000` ms | default explicit Teardown timeout |
| `client.server` | absolute HTTP(S) URL | no credentials/fragments | daemon endpoint |
| `client.timeout_ms` | unsigned integer | `1..3600000` ms | CLI wait deadline |
Environment mappings:
```text
OBS_LISTEN
OBS_CANONICAL_ORIGIN
OBS_STORAGE
OBS_MAX_STORED_BYTES
OBS_MAX_LIVE_ARTIFACTS
OBS_TEARDOWN_TIMEOUT_MS
OBS_SERVER
OBS_CLIENT_TIMEOUT_MS
```
### 8.2 Storage-limit accounting
`max_stored_bytes` counts the logical served-file bytes of every Observatory-owned Revision not yet physically discarded:
- current;
- superseded;
- recoverable;
- unavailable;
- quarantined pending final deletion; and
- source-copy staging reservations.
It excludes:
- `.obs.json`;
- recovery manifests;
- SQLite/WAL files;
- audit metadata;
- backups and recovery candidates; and
- external Service data.
This is the product’s Artifact-byte ceiling, not a claim about physical filesystem allocation. Diagnostics report it as `accountedStoredBytes`.
Each byte-adding operation reserves the exact sum of source regular-file lengths before visibility. The reservation remains counted until commit, failure cleanup, or quarantine classification. Replacement reserves the new Revision without subtracting the old current Revision.
`max_live_artifacts` counts Artifact identities in `live` state. Expired, deleted-recoverable, and gone Artifacts do not count. Replacement does not increase it; restore does.
Filesystem safety is independently enforced using available filesystem capacity. The operational reserve is:
```text
max(1 GiB, ceil(filesystem_total_bytes × 0.05))
```
Capacity responses report:
- `requiredBytes`;
- `accountedStoredBytes`;
- `maxStoredBytes`;
- `liveArtifacts`;
- `maxLiveArtifacts`;
- `filesystemAvailableBytes`;
- `reserveBytes`;
- `reclaimableBytes`;
- `blockingConstraint`.
No configured ceiling permits shortening recovery or evicting a live current Revision.
### 8.3 Target URL validation
A Target URL is valid only when all conditions hold:
1. parsing succeeds as an RFC 3986 absolute URI;
2. scheme is exactly `http` or `https`, case-insensitively normalized lowercase;
3. authority and nonempty host are present;
4. host is valid IPv4, bracketed IPv6, or IDNA-valid DNS name;
5. port, when present, is `1..65535`;
6. userinfo is absent, including empty userinfo syntax;
7. fragment is absent;
8. URL contains no ASCII control, space, CR, LF, tab, NUL, or invalid percent escape;
9. percent escapes are uppercase in the stored canonical form;
10. query parameter names, after percent-decoding and ASCII case folding, do not equal a forbidden credential name.
Forbidden query names are exactly:
```text
access_token
api_key
apikey
auth
authorization
bearer
credential
jwt
key
passwd
password
sig
signature
token
x-amz-credential
x-amz-signature
x-goog-credential
x-goog-signature
```
This makes the otherwise ambiguous “signed query credentials” prohibition testable. Unknown query parameter names are allowed; operators remain responsible for not storing credentials under misleading names.
URLs are stored and returned in canonical serialized form without attempting reachability, rewriting paths, removing ordinary queries, following redirects, or changing Service identity.
---
## 9. Browser UI mutation contract
### 9.1 Available browser mutations
The Project-led UI exposes only mutations that do not require selecting arbitrary daemon-host source paths or administering storage authority.
#### Project pages
- update Project title/slug;
- explicit Project register by typed daemon-host path;
- Project move;
- tombstone an empty Project.
#### Artifact details
- update title/description/slug;
- pin;
- unpin;
- remove into recovery;
- restore;
- preview and apply early permanent purge.
#### Service details
- update label/description/slug;
- replace or clear Teardown Action;
- pin/unpin;
- keep;
- refresh Service;
- remove catalogue metadata;
- confirmed teardown;
- add/update/remove/promote/refresh Targets.
#### Operations pages
- cleanup preview and confirmed run;
- normal status and diagnostics;
- view recovery and backup state and audit history.
The browser does **not** expose:
- Publish, replace, or import;
- deep diagnostics;
- recovery apply/resume;
- backup create/restore;
- candidate activation/discard;
- setup check/apply/remove/uninstall;
- service status/start/stop/restart;
- configuration installation.
Those remain CLI operator workflows because they require host-path selection, maintenance authority, or local service-management context.
### 9.2 Confirmation levels
| Mutation | Confirmation |
|---|---|
| Metadata update, pin/unpin, keep, refresh, non-primary Target add/update/promote | ordinary submit |
| Artifact remove/restore, Service remove, primary Target removal | review page showing resource, version, and effect |
| Teardown | type exact Service name and confirm process effect plus catalogue-removal condition |
| Project move/tombstone | type exact Project key and review Service/Artifact consequences |
| Cleanup run | exact preview summary and confirmation |
| Early purge | separately created exact plan, type Artifact key, permanent-loss warning |
No GET request mutates state. This claim applies to API and UI representations only: Artifact byte-serving GETs under `/artifacts/…` and `/revisions/…` serve bytes and are outside the API representation contract. Confirmation pages are themselves read-only.
### 9.3 Host, same-origin, and CSRF checks
Every browser mutation must pass all of:
1. request `Host`/HTTP authority exactly matches configured canonical origin;
2. `Origin` exactly equals the canonical origin;
3. when `Origin` is unavailable, `Referer` must parse to the exact canonical origin;
4. `Sec-Fetch-Site`, when supplied, must be `same-origin`;
5. a one-use, short-lived CSRF token must match the action, resource ID, record version, and confirmation plan;
6. the mutation must include `If-Match` and an idempotency key.
The CSRF token:
- is cryptographically random;
- is embedded in the server-rendered form;
- expires after ten minutes;
- is consumed only when dispatch is accepted;
- is not an account session or authentication cookie;
- cannot authorize a different action or newer record version.
Progressive-enhancement `fetch` requests send the same token as:
```http
X-Observatory-CSRF: <token>
```
and use the same application endpoint. Non-JavaScript forms post to `/ui/...` action routes which validate the same token and call the same application service, then return `303 See Other` to a server-returned canonical detail URL. The form route is an adapter, not an alternate lifecycle implementation.
Cross-origin, missing-source-origin, or mismatched-Host browser mutations return `403 browser_origin_rejected`. Loopback CLI calls without browser `Origin`/Fetch Metadata headers do not require a CSRF token but still require idempotency and conditional headers.
OWASP recommends CSRF tokens, origin verification, and Fetch Metadata as complementary defenses. [OWASP CSRF Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html)
---
## 10. Acceptance fixtures
The implementation must ship shared fixtures exercised against the application service, HTTP API, CLI adapter, and UI adapter.
### Fixture A — identity and URL authority
Given:
- uppercase ID;
- stale slug/current ID;
- unknown ID;
- tombstoned ID;
- same canonical directory through symlink and `..` spelling.
Assert:
- API malformed uppercase ID is `422`, with no redirect;
- browser GET safely canonicalizes uppercase ID/stale slug with one absolute `308`;
- unknown is `404`;
- tombstoned is `410`;
- Project resolve returns the same canonical directory and existing Project;
- every result uses server-returned absolute URLs;
- clients contain no route-construction fallback.
### Fixture B — Project allocation, move, and tombstone
1. Resolve unknown canonical directory: `unregistered`, no new row or ID.
2. Register: new random ID/key.
3. Register equivalent path: `409 already_exists`.
4. Move with incomplete Service enumeration: atomic failure.
5. Move with changed Service version: `412`.
6. Successful move:
   - new Project and Service IDs;
   - old IDs `410`;
   - new Target observations `unknown`;
   - no teardown execution;
   - Artifact Project association unchanged;
   - Artifact open URL unchanged.
7. Tombstone nonempty Project: `409`.
8. Tombstone empty Project: `410` thereafter, no redirect or ID reuse.
### Fixture C — ETag and idempotency
- GET Service returns `ETag: "rv-8"`.
- PATCH without `If-Match`: `428`.
- PATCH with stale ETag: `412 changed_record`.
- New key commits once.
- Same key/body replays identical resource and ETag.
- Same key/changed label: `409 idempotency_conflict`.
- Disconnect after durable commit, then identical retry: one effect and replay marker.
- Concurrent duplicate: no duplicated intent/effect.
### Fixture D — import mixed and zero-success
Mixed two-item import:
- HTTP `200`;
- `ok:true`;
- stdout only in CLI JSON mode;
- `overall:"partial"`;
- `partial:true`;
- ordered committed/failed items;
- exit `8`;
- successful sibling remains committed.
Zero-success two-item import:
- HTTP `200`;
- `ok:true`;
- `overall:"failed"`;
- `partial:false`;
- both trustworthy item errors in input order;
- stdout only;
- exit `8`.
Malformed empty import:
- `ok:false`;
- stderr only;
- `422`;
- exit `2`.
### Fixture E — Target URL corpus
Accept:
```text
https://host.example/
http://127.0.0.1:4173/session/current
https://[fd7a:115c:a1e0::1]:8443/a?view=full
https://host.example/a%20b?q=ordinary
```
Reject:
```text
ftp://host.example/
https://user@host.example/
https://host.example/#token
https://host.example/?access_token=x
https://host.example/?X-Amz-Signature=x
https://host.example/%ZZ
https://host.example:0/
https://host.example:65536/
https://host.example/\evil
```
Assert no probe, registration, log, or diagnostic leaks rejected credential material.
### Fixture F — teardown
Exercise:
- unavailable action;
- missing Project directory;
- launch error;
- exit `1`;
- signal termination;
- timeout at configured/default/override limit;
- successful exit `0`;
- concurrent Service update before removal.
Assert all failure/timeout/concurrent cases preserve the Service; only unchanged exit `0` tombstones it. Captured output is escaped, bounded, and truncation-labeled.
### Fixture G — Artifact lifecycle and storage ceilings
- default, explicit TTL, and pin transitions;
- removal enters seven-day recovery;
- stable URL returns `410`;
- restore chooses explicit mode;
- early purge requires matching unexpired plan and confirmation;
- `max_live_artifacts` blocks new Publish but not replacement;
- `max_stored_bytes` includes superseded/recoverable/quarantined Revision bytes and staging reservations;
- reserve calculation is exact;
- pressure cleanup never removes current live Revision or shortens recovery;
- capacity details expose every required accounting field.
### Fixture H — UI and CSRF
For each browser mutation:
- server-rendered form works without JavaScript;
- JS enhancement calls the same application operation;
- GET is inert;
- stale record token/ETag fails;
- wrong Host fails;
- cross-origin `Origin` fails;
- cross-site Fetch Metadata fails;
- missing/expired/replayed CSRF token fails;
- success returns canonical `303` for form flow;
- Service Open remains a direct Target URL;
- Artifact Open remains stable current URL;
- keyboard, focus, status announcements, 200% reflow, mobile cards, and reduced-motion behavior remain intact.
### Fixture I — diagnostics/recovery and result separation
- unhealthy completed diagnostics: `ok:true`, `health:"unhealthy"`, exit `10`;
- skipped requested checks: `ok:true`, `partial:true`, exit `8`;
- no trustworthy diagnostic context: `ok:false`, stderr;
- recovery preview makes no authority change;
- stale plan is rejected;
- apply cannot broaden selectors;
- retry resumes one intent;
- activation/restore/discard require matching plan and confirmation.
### Fixture J — API/CLI/UI parity
For every mutation exposed by more than one adapter, capture the canonical application request and compare:
- normalized operation kind;
- selected IDs;
- record version;
- idempotency fingerprint;
- validation outcome;
- audit event;
- resulting resource;
- canonical URLs;
- error code/status.
No adapter-specific lifecycle branch is permitted.

### Fixture K — local setup and service authority
- No `/api/v1/system/setup` or setup-apply route exists.
- Every setup and service leaf uses the common output envelope.
- Non-TTY mutations reject a missing idempotency key; an interactive TTY may generate and display one before dispatch.
- `setup check` is inert.
- `setup apply` verifies the installed deployment and exact daemon health without exposing an install API.
- `setup remove` removes only the owned Serve root handler and generated unit, retaining executables, stable command, configuration, and authoritative data.
- `setup uninstall` additionally removes only verified Observatory-owned executable integration and setup receipts, retaining configuration and authoritative data.
- Both removal leaves reject foreign or drifted paths and never purge catalogue or Artifact data.
- Service status/start/stop/restart preserve their fixed read, start-only, clean-stop, and verified-restart meanings without implicit installation.
- `system config validate FILE` rejects a missing, non-regular, or symlinked local FILE; sends only its TOML bytes to the daemon; has no stdin mode; returns ordered parse/schema/semantic checks; needs no mutation headers; never writes configuration; and returns daemon-unavailable exit `5` when appropriate.
- The configuration-validation API receives no client path and has no installation or activation effect.

---
## 11. Resolved source conflicts
### 11.1 Prototype batch envelope
The CLI prototype shows a mixed import with `ok:false` and a `result`. That conflicts with the authoritative one-envelope rule that `ok:false` is command-level failure and failure JSON goes to stderr.
**Resolution:** trustworthy per-item outcomes use `ok:true` on stdout. `overall`, `partial`, counts, and ordered item outcomes express mixed or zero-success batches. Exit `8` covers both mixed and zero-success trustworthy batches.
### 11.2 Prototype Project resolution allocation
The prototype says `project resolve` prints an “allocated key,” while the settled specification says allocation occurs on first catalogue creation and issue #17 explicitly asks whether resolution allocates.
**Resolution:** resolution never allocates. `project register` explicitly creates identity. Creation operations requiring a Project must fail `project_not_registered` and direct the caller to register; they do not hide Project creation inside a read.
No other fixed-source contradiction requires reopening a settled decision.
---
## 12. Sources
### Primary and controlling sources
- [Observatory `SPEC.md`](https://github.com/Whamp/observatory/blob/master/SPEC.md) — authoritative product contract.
- [Issue #17](https://github.com/Whamp/observatory/issues/17) — exact resolution scope.
- Approved CLI and Project-led index prototypes — vocabulary, output, and interaction evidence, subordinate to settled issue resolutions.
- [Persistence architecture](2026-07-09-observatory-persistence-architecture.md), [diagnostics and recovery](2026-07-09-observatory-storage-diagnostics-recovery.md), and [implementation stack](2026-07-09-observatory-implementation-stack.md) — authority, crash, recovery, setup, and embedded-UI constraints.
- [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110) — HTTP conditional and status semantics.
- [RFC 6585](https://www.rfc-editor.org/rfc/rfc6585) — `428 Precondition Required`.
- [RFC 8288](https://www.rfc-editor.org/rfc/rfc8288) — pagination links.
- [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785) — canonical JSON fingerprinting.
- [IETF Idempotency-Key draft](https://datatracker.ietf.org/doc/html/draft-ietf-httpapi-idempotency-key-header) — key, fingerprint, and replay model.
- [OWASP CSRF Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html) — token, Origin, and Fetch Metadata defenses.
### Rejected source models
- Generic REST/CRUD conventions obscure Observatory’s lifecycle-specific operations.
- WebDAV `207 Multi-Status` would add an unnecessary second batch model.
- RFC 9457 wire format conflicts with the settled Observatory envelope.
- Prototype behavior cannot override authoritative issue resolutions.
---
## 13. Residual implementation risks
These are proof obligations, not unresolved product decisions:
1. Correct request canonicalization and source-snapshot binding must be fault-injection tested.
2. Target URL parsing must use one pinned parser with the acceptance corpus; mixed parser behavior between CLI and daemon is prohibited.
3. CSRF nonce storage/expiry must not create an application session or bypass Tailscale authorization.
4. Project move must be tested as one short catalogue transaction and must not perform filesystem or process effects.
5. Issue #19 must implement the fixed setup/service leaves without changing their public semantics, crossing ownership boundaries, or introducing implicit installation/start behavior.
---
