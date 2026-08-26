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

To serve on port 3000 instead:

```bash
echo "EXTERNAL_PORT=3000" >> .env
docker compose up -d
```

`BIND_ADDRESS` is also honoured by the binary, but leave it alone under Docker — the container must bind `0.0.0.0:8080` for port publishing to work. It's there for running outside a container.

These are compile-time constants in `src/main.rs`:

| Constant | Value |
|---|---|
| `PIN_LENGTH` | 4 characters |
| `MAX_RESULT_SIZE_BYTES` | 3000 bytes |
| `STALE_AGE_MINS` | 10 minutes |
| `CLEANUP_INTERVAL` | 10 seconds |

## API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/pin/{namespace}` | Allocate a new PIN |
| `POST` | `/pin/{namespace}/{pin}` | Poll a PIN; consumes the payload if present |
| `PUT` | `/pin/{namespace}/{pin}` | Submit a payload to an existing PIN |
| `GET` | `/health` | Health check |

Namespaces are just a key prefix — the same PIN string in two namespaces refers to two unrelated slots. No setup needed; use any string.

### Allocate a PIN

```bash
curl -X POST http://localhost:8080/pin/myapp
# 200 {"pin":"A7X9","result":null}
```

Returns `429` if it can't find a free PIN in 10 attempts.

### Poll a PIN

```bash
curl -X POST http://localhost:8080/pin/myapp/A7X9
```

Always `200`, but there are three cases to distinguish, and they're easy to confuse:

| Situation | Response | What it means |
|---|---|---|
| PIN exists, has a payload | `{"pin":"A7X9","result":{...}}` | Delivered. The PIN is now destroyed. |
| PIN exists, still empty | `{"pin":"A7X9","result":null}` | Same PIN echoed back. Keep polling it. |
| PIN unknown or expired | `{"pin":"B2Y4","result":null}` | **A different PIN.** Your old one is gone; start over with this one. |

So a client loop must re-read `pin` from every response rather than assuming it's unchanged — that's how expiry is signalled.

### Submit a payload

```bash
curl -X PUT http://localhost:8080/pin/myapp/A7X9 \
  -H "Content-Type: application/json" \
  -d '{"message": "Hello, World!", "n": 42}'
# 202 Thanks!
```

The body must be a **JSON object**. Arrays and scalars are rejected with `422`. Submitting also resets the PIN's 10-minute expiry clock.

### Health

```bash
curl http://localhost:8080/health
# 200 All good.
```

### Status codes

| Code | Meaning |
|---|---|
| `200` | PIN allocated, or poll answered |
| `202` | Payload accepted |
| `404` | PIN doesn't exist or expired (on `PUT`) |
| `413` | Payload over 3000 bytes |
| `415` | Missing `Content-Type: application/json` |
| `422` | Body wasn't a JSON object |
| `429` | Couldn't allocate a free PIN |

## Example: polling client

```bash
#!/bin/bash
NAMESPACE="myapp"

PIN=$(curl -s -X POST "http://localhost:8080/pin/$NAMESPACE" | jq -r '.pin')
echo "Waiting for data on PIN: $PIN"

while true; do
    RESPONSE=$(curl -s -X POST "http://localhost:8080/pin/$NAMESPACE/$PIN")
    RESULT=$(jq -r '.result' <<< "$RESPONSE")

    if [ "$RESULT" != "null" ]; then
        echo "Data received: $RESULT"
        break
    fi

    # The service hands back a new PIN when the old one expires.
    NEW_PIN=$(jq -r '.pin' <<< "$RESPONSE")
    if [ "$NEW_PIN" != "$PIN" ]; then
        PIN=$NEW_PIN
        echo "PIN expired, now using: $PIN"
    fi

    sleep 2
done
```

## Local development

Needs Rust 1.85+ (edition 2024).

```bash
cargo run                              # serves on 0.0.0.0:8080
BIND_ADDRESS=127.0.0.1:3000 cargo run  # or somewhere else

cargo test          # 19 unit + integration tests
cargo clippy --all-targets
cargo fmt
```

### Layout

| File | Purpose |
|---|---|
| `src/main.rs` | The whole service: handlers, storage, cleanup, tests |
| `Dockerfile` | Multi-stage build; dependency layer cached separately from source |
| `docker-compose.yml` | Deployment definition |
| `deploy.sh` | Convenience wrapper around `docker compose` |

### Dependencies

| Crate | Purpose |
|---|---|
| `axum` | HTTP server |
| `dashmap` | Sharded concurrent map holding the PINs |
| `tokio` | Async runtime; also drives the cleanup interval |
| `serde` / `serde_json` | JSON handling |
| `chrono` | Expiry timestamps |
| `rand` | PIN generation |
| `tower-http` | CORS |
| `env_logger` / `log` | Logging |
| `dotenvy` | Loads `.env` when running outside Docker |
| `anyhow` | Error handling in `main` |

## Limitations

- **In memory only.** A restart drops every PIN and payload. `docker compose up -d --build` therefore loses in-flight exchanges.
- **No authentication.** Anyone who reaches the port can allocate PINs and read any PIN they guess.
- **PINs are short.** 4 characters is roughly 1.6M combinations, and a namespace with many live PINs is brute-forceable. Use unguessable namespaces if that matters.
- **No rate limiting.** Put it behind a reverse proxy if it's exposed.
- **CORS is fully permissive**, so any origin can call it from a browser.

Deploy it behind a proxy that terminates TLS and adds auth if it's going anywhere public.
