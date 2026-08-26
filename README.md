# Configgymajiggy

> ⚠️ **Warning**: Do not use this. I only made it public so I could hurt myself and deploy it to some hacky cloud instance easier, previously I was covering my shame by making it private.

A small Rust service for handing data between two systems using a short PIN. One side asks for a PIN, the other side PUTs a JSON payload against it, and the first side polls until it comes back. Everything lives in memory and expires after 10 minutes.

## How it works

```
Receiver                          Service                       Sender
   │  POST /pin/myapp                │                             │
   │────────────────────────────────>│                             │
   │  {"pin":"A7X9","result":null}   │                             │
   │<────────────────────────────────│    PUT /pin/myapp/A7X9      │
   │                                 │<────────────────────────────│
   │  POST /pin/myapp/A7X9           │            202 Thanks!      │
   │────────────────────────────────>│────────────────────────────>│
   │  {"pin":"A7X9","result":{...}}  │
   │<────────────────────────────────│   ← payload delivered, PIN destroyed
```

A payload is delivered to exactly one poller. Reading it destroys the PIN.

## Deploying

Requires Docker with the Compose v2 plugin (`docker compose`, not the old `docker-compose` binary).

```bash
git clone <repository-url>
cd configgy

cp .env.example .env   # optional, see Configuration
docker compose up -d --build
```

That builds the image and starts the service on port 8080 with `restart: unless-stopped`, so it comes back on reboot.

The published port binds **every host interface**, not just loopback — `localhost` in the examples below is just the most convenient address, not a limit on who can reach it. There is no authentication, so put it behind a proxy before exposing the host (see [Limitations](#limitations)).

Verify it:

```bash
curl http://localhost:8080/health          # -> All good.
docker compose ps                          # STATUS should read "healthy"
```

The container has a built-in healthcheck, so `docker compose ps` reports real service health rather than just "the process is running". It takes a few seconds after startup to flip from `starting` to `healthy`.

### Redeploying

```bash
git pull
docker compose up -d --build
```

Compose rebuilds and replaces the container only if something changed. The Dockerfile caches the dependency build separately from your source, so source-only changes rebuild in seconds.

### Day-to-day

```bash
docker compose logs -f configgymajiggy   # tail logs
docker compose restart                   # restart
docker compose down                      # stop and remove
docker compose down && docker compose up -d --build   # force full recreate
```

`./deploy.sh` wraps these same commands (`deploy`, `start`, `stop`, `restart`, `logs`, `status`, `update`, `clean`) if you prefer.

## Configuration

Copy `.env.example` to `.env`. Compose reads it automatically; both are optional and sensible defaults apply.

| Variable | Default | Purpose |
|---|---|---|
| `RUST_LOG` | `info` | Log level: `error`, `warn`, `info`, `debug`, `trace` |
| `EXTERNAL_PORT` | `8080` | Host port to publish on. The container always listens on 8080 internally. |
| `PIN_LENGTH` | `4` | Characters per PIN. Each one adds 5 bits. |
| `STALE_AGE_MINS` | `10` | Minutes of inactivity before a PIN is evicted. |
| `MAX_PAYLOAD_BYTES` | `3000` | Largest `PUT` body accepted, measured on the wire. |
| `MAX_ENTRIES` | `100000` | Hard cap on live PINs; allocation returns `503` past it. |
| `CLEANUP_INTERVAL_SECS` | `10` | How often the expiry sweep runs. |
| `REQUEST_TIMEOUT_SECS` | `30` | Per-request timeout. |
| `CORS_ALLOWED_ORIGINS` | *(empty)* | Comma-separated origin allowlist. Empty means any origin. |

To serve on port 3000 instead:

```bash
echo "EXTERNAL_PORT=3000" >> .env
docker compose up -d
```

`BIND_ADDRESS` is also honoured by the binary, but leave it alone under Docker — the container must bind `0.0.0.0:8080` for port publishing to work. It's there for running outside a container.

Invalid values are rejected at startup rather than silently falling back.

## API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/pin/{namespace}` | Allocate a new PIN |
| `POST` | `/pin/{namespace}/{pin}` | Poll a PIN; consumes the payload if present |
| `PUT` | `/pin/{namespace}/{pin}` | Submit a payload to an existing PIN |
| `GET` | `/health` | Health check |

Namespaces partition the PIN space — the same PIN string in two namespaces refers to two unrelated slots. No setup needed, but a namespace must match `[A-Za-z0-9_-]{1,64}`; anything else is a `400`. PINs are drawn from the Crockford base32 alphabet (`0-9`, `A-Z` minus `I`, `L`, `O` and `U`) and are matched case-insensitively, so `a7x9` and `A7X9` are the same PIN.

### Allocate a PIN

```bash
curl -X POST http://localhost:8080/pin/myapp
# 200 {"pin":"A7X9","result":null}
```

Returns `503` with a `Retry-After` header if the namespace is saturated (10 allocation attempts collided) or the service is at `MAX_ENTRIES`.

### Poll a PIN

```bash
curl -X POST http://localhost:8080/pin/myapp/A7X9
```

Three cases:

| Situation | Response | What it means |
|---|---|---|
| PIN exists, has a payload | `200 {"pin":"A7X9","result":{...}}` | Delivered. The PIN is now destroyed. |
| PIN exists, still empty | `200 {"pin":"A7X9","result":null}` | Keep polling. This also resets the expiry clock. |
| PIN unknown or expired | `404` | Your PIN is gone. Allocate a new one. |

Polling keeps a PIN alive, so a client that keeps polling never has its PIN expire underneath it. A `404` means the PIN really is gone — allocate a fresh one with `POST /pin/{namespace}`.

### Submit a payload

```bash
curl -X PUT http://localhost:8080/pin/myapp/A7X9 \
  -H "Content-Type: application/json" \
  -d '{"message": "Hello, World!", "n": 42}'
# 202 Thanks!
```

The body must be a **JSON object**. Arrays and scalars are rejected with `422`. Submitting also resets the PIN's expiry clock.

A PIN holds one payload: submitting to a PIN that already has an undelivered payload returns `409` rather than overwriting it.

### Health

```bash
curl http://localhost:8080/health
# 200 All good.
```

### Status codes

Errors carry a JSON body of the form `{"error": "..."}`.

| Code | Meaning |
|---|---|
| `200` | PIN allocated, or poll answered |
| `202` | Payload accepted |
| `400` | Malformed namespace or PIN |
| `404` | PIN doesn't exist or expired |
| `409` | PIN already holds an undelivered payload |
| `413` | Body over `MAX_PAYLOAD_BYTES` |
| `422` | Body wasn't a JSON object |
| `503` | No PIN available; retry after the `Retry-After` interval |

## Example: polling client

```bash
#!/bin/bash
NAMESPACE="myapp"

PIN=$(curl -s -X POST "http://localhost:8080/pin/$NAMESPACE" | jq -r '.pin')
echo "Waiting for data on PIN: $PIN"

while true; do
    RESPONSE=$(curl -s -w '\n%{http_code}' -X POST "http://localhost:8080/pin/$NAMESPACE/$PIN")
    STATUS=$(tail -n1 <<< "$RESPONSE")
    BODY=$(sed '$d' <<< "$RESPONSE")

    if [ "$STATUS" = "404" ]; then
        # Expired. Polling normally keeps it alive, so this only happens after
        # a real gap - or a service restart.
        PIN=$(curl -s -X POST "http://localhost:8080/pin/$NAMESPACE" | jq -r '.pin')
        echo "PIN expired, now using: $PIN"
        continue
    fi

    RESULT=$(jq -r '.result' <<< "$BODY")
    if [ "$RESULT" != "null" ]; then
        echo "Data received: $RESULT"
        break
    fi

    sleep 2
done
```

## Local development

Needs Rust 1.85+ (edition 2024).

```bash
cargo run                              # serves on 0.0.0.0:8080
BIND_ADDRESS=127.0.0.1:3000 cargo run  # or somewhere else

cargo test                                     # 42 unit + integration tests
cargo clippy --all-targets -- -D warnings      # pedantic; clean
cargo fmt
```

### Layout

| File | Purpose |
|---|---|
| `src/main.rs` | Startup: config, sweeper task, serve, graceful shutdown, `--health-check` |
| `src/config.rs` | `Config`, read and validated from the environment once at startup |
| `src/pin.rs` | `Namespace` and `Pin` newtypes — the alphabet, generation, validation |
| `src/store.rs` | `PinStore`. Every lock taken against the map lives in this file |
| `src/handlers.rs` | The four handlers and the router |
| `src/error.rs` | `ApiError` and its JSON representation |
| `tests/api.rs` | End-to-end tests through the router |
| `Dockerfile` | Multi-stage build; dependency layer cached separately from source |
| `docker-compose.yml` | Deployment definition |
| `deploy.sh` | Convenience wrapper around `docker compose` |

Untrusted input is confined to `pin.rs`: a `Namespace` or `Pin` value only exists
if it parsed, so handlers downstream cannot see an unvalidated one.

### Dependencies

| Crate | Purpose |
|---|---|
| `axum` | HTTP server |
| `dashmap` | Sharded concurrent map holding the PINs |
| `tokio` | Async runtime; also drives the cleanup interval |
| `serde` / `serde_json` | JSON handling |
| `rand` | PIN generation |
| `tower-http` | CORS |
| `env_logger` / `log` | Logging |
| `dotenvy` | Loads `.env` when running outside Docker |

## Limitations

- **In memory only.** A restart drops every PIN and payload. `docker compose up -d --build` therefore loses in-flight exchanges.
- **No authentication.** Anyone who reaches the port can allocate PINs and read any PIN they guess.
- **PINs are short.** The alphabet is a uniform 32 symbols, so the default 4 characters is 20 bits — about 1.05M combinations. That is brute-forceable in minutes by anyone who can reach the port, and there is no rate limiting here to stop them. Raise `PIN_LENGTH` (each character is another 5 bits) when the PINs guard anything worth stealing; 4 is the default because the point of the service is a code a human can read aloud.
- **No rate limiting.** Put it behind a reverse proxy if it's exposed.
- **CORS defaults to any origin.** Set `CORS_ALLOWED_ORIGINS` to restrict it.
- **Memory is bounded only by `MAX_ENTRIES` and the TTL.** A burst of allocations holds memory for `STALE_AGE_MINS`; the compose file caps the container at 512 MB.

Deploy it behind a proxy that terminates TLS and adds auth if it's going anywhere public.
