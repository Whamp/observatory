# Tailscale Serve for linked live Services

Date: 2026-07-09

Ticket: [Validate linked live Services through Tailscale Serve](https://github.com/Whamp/observatory/issues/4)

## Verdict

**The exact Sideshow case fails today.** Serve's transport worked across separate probes: Sideshow's root routes and SSE passed from the host and a remote tailnet client, and a loopback-only control backend passed WebSocket tests. But unmodified Sideshow 0.7.0 was not loopback-only and did not preserve its public HTTPS origin. No single tested application simultaneously satisfied every premise in the question.

Two Sideshow defects prevent one reliable, safely loopback-only Service URL:

1. `sideshow serve` says it listens on `localhost`, but its socket listens on every interface.
2. Behind Serve's HTTPS terminator, Sideshow generates absolute `http://` URLs because it ignores `X-Forwarded-Proto: https`.

Observatory should keep the link-first Service design and should not add a proxy. Before adopting Sideshow as a canonical Service, Sideshow needs a real loopback bind option and proxy-aware external-origin handling.

## Tested environment

| Component | Observed value |
| --- | --- |
| Serve host | `desktop` |
| Tailscale | 1.98.8, commit `1241b225bc798707d02db3570992625d3a16594f` |
| MagicDNS name | `desktop.greyhound-chinstrap.ts.net` |
| Tailnet address | `100.112.72.93` |
| Existing persistent Serve route | HTTPS 443 → `http://127.0.0.1:3773` |
| Sideshow | npm package 0.7.0 |
| Remote tailnet client | `server60`, Tailscale 1.98.8 |

The tests added foreground-only Serve listeners. Each listener disappeared on process exit. A canonical JSON comparison confirmed that the persistent Serve configuration was identical before and after every successful probe.

The durable command and output record is [Probe transcript: Tailscale Serve and Sideshow](evidence/2026-07-09-tailscale-serve-probe.md).

## Live results

### Root-oriented Sideshow routes

A temporary Sideshow instance served through:

```text
https://desktop.greyhound-chinstrap.ts.net:8443
    → Tailscale Serve
    → http://127.0.0.1:18228
```

The Serve host and `server60` both received HTTP 200 from every tested route:

| Route | Host | Remote tailnet client |
| --- | ---: | ---: |
| `/` | 200 | 200 |
| `/session/:sessionId` | 200 | 200 |
| `/session/:sessionId/s/:surfaceId` | 200 | 200 |
| `/s/:surfaceId?part=0` | 200 | 200 |
| `/api/version` | 200 | 200 |
| `/api/surfaces/:surfaceId` | 200 | 200 |

The rendered `/s/:surfaceId` document contained the published HTML. A root Serve mount preserved Sideshow's root-relative viewer and API routes.

This matches Tailscale 1.98.8's implementation: Serve retains the incoming host at a root proxy and sends requests through Go's `httputil.ReverseProxy`. See [`ipn/ipnlocal/serve.go`](https://github.com/tailscale/tailscale/blob/v1.98.8/ipn/ipnlocal/serve.go#L927-L949) and the [Serve CLI reference](https://tailscale.com/docs/reference/tailscale-cli/serve).

### SSE

Sideshow 0.7.0 uses SSE at `/api/events`. The host and `server60` each:

1. opened the event stream through the MagicDNS HTTPS URL;
2. received Sideshow's initial `hello` event;
3. posted a comment through the same HTTPS origin; and
4. received the resulting `comment-created` event on the open stream.

Observed stream shape:

```text
event: hello
data: {}

data: {"type":"comment-created", ...}
```

Sideshow's source implements this endpoint with Hono's `streamSSE`: [`server/app.ts`](https://github.com/modem-dev/sideshow/blob/f26248331010f3d7be2805765ff1ace32df5545f/server/app.ts). The live test establishes event delivery through Serve; it did not run a multi-hour reconnect or idle-longevity test.

### WebSockets

Sideshow 0.7.0's normal viewer uses SSE, not WebSockets. A separate loopback-only WebSocket echo backend tested Serve's upgrade path on HTTPS port 8444.

Both clients completed a TLS connection, an HTTP 101 upgrade, a masked text frame, and an echoed text frame:

```text
host_websocket_echo=true
remote_websocket_echo=true
```

This proves a complete WebSocket handshake and frame exchange through the installed Serve version. It does not establish multi-hour idle-connection behavior.

### HTTPS ports and certificates

The persistent listener on 443 remained active while foreground listeners were tested on 8443 and 8444. Each port presented the same certificate:

```text
subject=CN=desktop.greyhound-chinstrap.ts.net
issuer=C=US, O=Let's Encrypt, CN=E7
serial=06F4A3C6E4D1BF8EA3D28514138A20882631
SAN=DNS:desktop.greyhound-chinstrap.ts.net
```

A certificate identifies a DNS name, not a port. Extra Serve listeners therefore use the same MagicDNS name and certificate, but their URLs must include `:PORT`. See Tailscale's [HTTPS certificate documentation](https://tailscale.com/docs/how-to/set-up-https-certificates) and [Serve documentation](https://tailscale.com/docs/features/tailscale-serve).

The test exercised 8443 and 8444 sequentially, each concurrently with persistent 443. It did not empirically run 8443 and 8444 together. Tailscale's configuration model supports multiple host-and-port handlers; production configuration should define them together.

## Sideshow blockers

### `sideshow serve` is not loopback-only

Sideshow 0.7.0 printed:

```text
sideshow listening on http://localhost:18228
```

The actual socket was:

```text
LISTEN *:18228
```

The package calls Hono's server without a hostname:

```js
serve({ fetch: app.fetch, port }, ...)
```

Source: [`server/index.ts` at Sideshow 0.7.0](https://github.com/modem-dev/sideshow/blob/f26248331010f3d7be2805765ff1ace32df5545f/server/index.ts).

Serve can target `127.0.0.1:18228`, but that target string does not make Sideshow's own wildcard listener private. Sideshow needs a `--host` or equivalent setting that passes `127.0.0.1` to its HTTP server.

### Sideshow generates the wrong external scheme

Tailscale Serve terminates TLS, then proxies HTTP to the local backend. It sends:

```text
X-Forwarded-Host: desktop.greyhound-chinstrap.ts.net:8443
X-Forwarded-Proto: https
```

Source: [`addProxyForwardedHeaders` in Tailscale 1.98.8](https://github.com/tailscale/tailscale/blob/v1.98.8/ipn/ipnlocal/serve.go#L1036-L1043).

Sideshow ignores those headers and derives its public origin from the backend request URL:

```js
new URL(c.req.url).origin
```

That request is HTTP between Serve and Sideshow. The live HTTPS asset upload returned:

```text
generated_asset_url=http://desktop.greyhound-chinstrap.ts.net:8443/a/<asset-id>
```

Requesting that generated URL returned HTTP 400 because port 8443 expects TLS. Replacing only the scheme with `https://` returned 200. `/setup` also emitted the wrong `http://desktop...:8443` origin, and rendered documents used the wrong HTTP origin in generated policy data.

The affected origin construction appears in [`server/app.ts` at Sideshow 0.7.0](https://github.com/modem-dev/sideshow/blob/f26248331010f3d7be2805765ff1ace32df5545f/server/app.ts). It remains in [upstream `main` at `bf0dd67`](https://github.com/modem-dev/sideshow/blob/bf0dd67fab3a695aeddde599ffae9c55d4f8fcb8/server/app.ts).

Sideshow should either:

- trust Serve's forwarded host and scheme under an explicit trusted-proxy setting; or
- accept one explicit canonical external origin, such as `SIDESHOW_PUBLIC_URL`.

Blindly trusting forwarded headers from arbitrary direct clients would create a host-header spoofing risk. The trust boundary must be explicit and pair with a loopback-only listener.

## Decision for Observatory

1. Keep a Service as a link to a separately running application. Observatory does not proxy Sideshow.
2. Use a root Serve mount for root-oriented Services. Do not mount Sideshow below an Observatory path.
3. A non-443 Service URL must retain its explicit HTTPS port, for example `https://desktop.greyhound-chinstrap.ts.net:8443/`.
4. Require Service-specific validation before marking a URL canonical:
   - the process actually binds loopback;
   - root routes load;
   - generated absolute URLs preserve HTTPS and the public host;
   - the live update transport works from the host and a remote tailnet client;
   - the persistent Serve configuration survives restart and rollback checks.
5. Do not adopt unmodified Sideshow 0.7.0 as the canonical example. First fix its bind and external-origin behavior upstream or in a Sideshow-specific launch boundary owned outside Observatory.

## Reproduction evidence

[Probe transcript: Tailscale Serve and Sideshow](evidence/2026-07-09-tailscale-serve-probe.md) preserves the commands, sanitized outputs, SSE sequence, WebSocket handshake and frame assertions, generated-origin failure, certificate observations, and before/after Serve configuration comparison.

No persistent network configuration changed.
