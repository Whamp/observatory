# Probe transcript: Tailscale Serve and Sideshow

Date: 2026-07-09

Parent report: [Tailscale Serve for linked live Services](../2026-07-09-tailscale-serve-live-services.md)

This transcript preserves the decisive commands and sanitized outputs from `desktop`. The probes used foreground-only Serve listeners and temporary application data. Dynamic process IDs, Sideshow session IDs, surface IDs, asset IDs, and foreground-config IDs have no continuing authority.

## Safety check

The persistent configuration before every probe was:

```json
{
  "TCP": {
    "443": { "HTTPS": true }
  },
  "Web": {
    "desktop.greyhound-chinstrap.ts.net:443": {
      "Handlers": {
        "/": { "Proxy": "http://127.0.0.1:3773" }
      }
    }
  }
}
```

Each probe captured a canonical form before starting:

```bash
before=$(tailscale serve status --json | jq -S -c .)
```

It killed and waited for the foreground Serve process, then compared the state again:

```bash
kill "$serve_pid"
wait "$serve_pid"
after=$(tailscale serve status --json | jq -S -c .)
test "$before" = "$after"
```

Every completed probe reported:

```text
serve_config_restored=true
probe_exit=0
```

A final independent check showed only the original 443 route and no listeners on temporary ports 18228, 18229, 8443, or 8444.

## Machine and installed configuration

Commands:

```bash
hostname
tailscale version
tailscale status --json | jq '{Self, MagicDNSSuffix, CurrentTailnet}'
tailscale serve status --json
```

Relevant output:

```text
desktop
1.98.8
Tailscale commit: 1241b225bc798707d02db3570992625d3a16594f-dirty
MagicDNS: desktop.greyhound-chinstrap.ts.net
Tailscale IPv4: 100.112.72.93
Current Serve target: HTTPS 443 → http://127.0.0.1:3773
```

The remote client check used:

```bash
ssh server60 'hostname; tailscale version | head -1; getent hosts desktop.greyhound-chinstrap.ts.net'
```

Output:

```text
server60
1.98.8
100.112.72.93 desktop.greyhound-chinstrap.ts.net
```

## Sideshow bind check

The probe ran the exact installed npm package with isolated storage:

```bash
PKG=/home/will/.npm/_npx/af43ddcd6cfbe6b4/node_modules/sideshow
PORT=18228 \
SIDESHOW_DATA="$WORK/data.json" \
SIDESHOW_VERSION= \
node "$PKG/dist/server/index.js"
```

Sideshow logged:

```text
sideshow listening on http://localhost:18228
```

Socket inspection:

```bash
ss -ltnp 'sport = :18228'
```

Output:

```text
LISTEN 0 511 *:18228 *:* users:(("MainThread",pid=<pid>,fd=21))
```

The process therefore listened on every interface, not loopback.

## Root routes and SSE through HTTPS 8443

The probe created a Sideshow HTML surface through the loopback URL:

```bash
curl -fsS -X POST http://127.0.0.1:18228/api/surfaces \
  -H 'content-type: application/json' \
  --data '{
    "title":"Observatory Issue 4 probe",
    "sessionTitle":"Tailscale Serve validation",
    "agent":"observatory",
    "parts":[{"kind":"html","html":"<h1 id=probe>root-route probe</h1>"}]
  }'
```

It then started one foreground-only Serve listener:

```bash
tailscale serve --https=8443 --yes http://127.0.0.1:18228
```

Live Serve state contained the original persistent 443 route plus:

```json
{
  "TCP": {
    "8443": { "HTTPS": true }
  },
  "Web": {
    "desktop.greyhound-chinstrap.ts.net:8443": {
      "Handlers": {
        "/": { "Proxy": "http://127.0.0.1:18228" }
      }
    }
  }
}
```

The host and `server60` each ran `curl -fsS` against these URLs. `curl` performed normal certificate and hostname verification; no insecure flag was used.

```text
Route                                  desktop  server60
/                                      200      200
/session/:sessionId                    200      200
/session/:sessionId/s/:surfaceId       200      200
/s/:surfaceId?part=0                   200      200
/api/version                           200      200
/api/surfaces/:surfaceId               200      200
```

The rendered surface response contained:

```html
<h1 id=probe>root-route probe</h1>
```

The SSE sequence on each machine was:

```bash
timeout 6 curl -fsS -N \
  "$BASE/api/events?session=$session_id" >events.txt &
sse_pid=$!
sleep 1
curl -fsS -X POST "$BASE/api/comments" \
  -H 'content-type: application/json' \
  --data "{\"surface\":\"$surface_id\",\"author\":\"user\",\"text\":\"sse-probe\"}"
wait "$sse_pid" || true
grep -q 'event: hello' events.txt
grep -q 'comment-created' events.txt
```

Both streams contained:

```text
event: hello
data: {}

data: {"type":"comment-created","id":"<id>","sessionId":"<session>","surfaceId":"<surface>","seq":<n>}
```

Assertions:

```text
host_sse=true
remote_sse=true
host_magicdns_https=true
remote_magicdns_https=true
```

## Generated-origin failure

With the same 8443 topology, the probe uploaded an asset through the public HTTPS URL:

```bash
printf 'asset-probe' >asset.txt
curl -fsS -X POST \
  'https://desktop.greyhound-chinstrap.ts.net:8443/api/assets?kind=file&filename=probe.txt' \
  -H 'content-type: application/octet-stream' \
  --data-binary @asset.txt
```

Sideshow returned:

```json
{
  "kind": "file",
  "byteLength": 11,
  "filename": "probe.txt",
  "url": "http://desktop.greyhound-chinstrap.ts.net:8443/a/<asset-id>"
}
```

Results:

```text
generated_asset_url=http://desktop.greyhound-chinstrap.ts.net:8443/a/<asset-id>
generated_asset_url status=400
scheme_corrected_asset_url=https://desktop.greyhound-chinstrap.ts.net:8443/a/<asset-id> status=200
setup_origin=http://desktop.greyhound-chinstrap.ts.net:8443
```

The rendered `/s/:surfaceId` response also contained this generated origin:

```text
rendered_origin_ref=http://desktop.greyhound-chinstrap.ts.net:8443
```

## WebSocket through HTTPS 8444

The WebSocket control backend explicitly bound loopback:

```js
import http from "node:http";
import { WebSocketServer } from "ws";

const server = http.createServer((request, response) => {
  response.writeHead(200);
  response.end("ok");
});
const sockets = new WebSocketServer({ server, path: "/ws" });
sockets.on("connection", (socket) => {
  socket.on("message", (data, isBinary) => socket.send(data, { binary: isBinary }));
});
server.listen(18229, "127.0.0.1");
```

Socket inspection confirmed:

```text
LISTEN 0 511 127.0.0.1:18229 0.0.0.0:* users:(("MainThread",pid=<pid>,fd=21))
```

The foreground listener was:

```bash
tailscale serve --https=8444 --yes http://127.0.0.1:18229
```

A host-side Node client connected to:

```text
wss://desktop.greyhound-chinstrap.ts.net:8444/ws
```

It sent the text frame `host-echo` and required the same echoed text.

On `server60`, a Python standard-library client:

1. opened a verified TLS socket with the MagicDNS name as SNI;
2. sent an RFC 6455 upgrade request with a random `Sec-WebSocket-Key`;
3. required HTTP 101 and the correct `Sec-WebSocket-Accept` value;
4. sent a masked text frame containing `remote-echo`; and
5. required an unmasked text frame containing `remote-echo`.

Assertions:

```text
host_websocket_echo=true
remote_websocket_echo=true
```

## Certificate observations

For 443, 8443, and 8444, the probe used:

```bash
echo | openssl s_client \
  -connect "100.112.72.93:$port" \
  -servername desktop.greyhound-chinstrap.ts.net 2>/dev/null \
  | openssl x509 -noout -subject -issuer -serial -dates -ext subjectAltName
```

Every tested port presented:

```text
subject=CN=desktop.greyhound-chinstrap.ts.net
issuer=C=US, O=Let's Encrypt, CN=E7
serial=06F4A3C6E4D1BF8EA3D28514138A20882631
notBefore=May 24 01:57:01 2026 GMT
notAfter=Aug 22 01:57:00 2026 GMT
X509v3 Subject Alternative Name:
    DNS:desktop.greyhound-chinstrap.ts.net
```

The successful `curl -fsS` requests from both machines independently exercised normal TLS chain and hostname verification on 8443. The WebSocket clients did the same on 8444.
