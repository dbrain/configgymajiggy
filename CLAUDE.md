# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a Rust web service called "biboop" that provides a PIN-based temporary data exchange system. One side asks for a short PIN, the other side PUTs a JSON payload against it, and the first side polls until it comes back. Everything lives in memory and expires after a period of inactivity.

## Architecture

- **Framework**: Axum 0.8 for HTTP server
- **Storage**: In-memory `DashMap` (sharded concurrent map) keyed by a structured `PinKey`
- **Expiry**: A `tokio::time::interval` task sweeps entries untouched for `STALE_AGE_MINS`
- **Concurrency**: `Arc<DashMap<..>>` behind `PinStore`; no global write lock

### Module layout

| File | Contents |
|---|---|
| `src/main.rs` | Startup: config, supervised sweeper, serve, graceful shutdown, `--health-check` probe |
| `src/config.rs` | `Config`, read and validated from the environment once at startup |
| `src/pin.rs` | `Namespace` and `Pin` newtypes: alphabet, generation, validation |
| `src/store.rs` | `PinStore` — **every lock taken against the maps lives in this file** |
| `src/handlers.rs` | The four handlers and the router |
| `src/error.rs` | `ApiError` and its JSON representation |
| `tests/api.rs` | End-to-end tests through the router |
| `.github/workflows/ci.yml` | fmt, clippy, tests, release build, container smoke test |

Two invariants worth preserving:

- **Untrusted input is confined to `pin.rs`.** A `Namespace` or `Pin` value only exists if it parsed, so nothing downstream can hold an unvalidated one. The map key is a struct, not a joined string — `("a", "b:c")` and `("a:b", "c")` cannot collide.
- **All locking is in `store.rs`.** `poll` reads, refreshes the TTL, and removes under a single shard write lock via `remove_if_mut`, so concurrent pollers can never both receive the same payload and a write landing mid-poll is never lost. `sweep` does nothing but compare timestamps inside `retain`'s closure — that closure runs under a shard write lock, so any I/O there (including logging) would stall requests.
- **The sweeper is supervised.** `supervised_sweeper` respawns the loop if it panics; `/ready` catches the case where it stops anyway.
- **Two maps, never locked together.** `pins` holds the exchanges; `namespaces` holds per-namespace bookkeeping (quota counter and failed-guess counter). Every path takes them sequentially and drops one guard before acquiring the other — nesting them would risk a deadlock. `allocate` claims the quota slot, drops the guard, then inserts the pin, releasing the slot if insertion fails.

### API Endpoints

- `POST /pin/{namespace}`: Allocate a new PIN
- `POST /pin/{namespace}/{pin}`: Poll. Returns the same PIN with `result: null` while empty (refreshing its expiry), the payload once submitted, or `404` if the PIN is unknown or expired. It never allocates.
- `PUT /pin/{namespace}/{pin}`: Submit a payload. `409` if one is already pending.
- `GET /health`: Liveness. Does no work.
- `GET /ready`: Readiness. `503` if the expiry sweep is overdue, which is what a dead sweeper looks like from outside. The container healthcheck probes this, not `/health`.

`POST /pin/{namespace}/{pin}` accepts `?wait=<seconds>` (clamped to `MAX_LONG_POLL_SECS`) to long-poll: the handler registers on the pin's `Notify` *before* reading the slot, so a payload landing between the read and the wait cannot be missed.

## Development Commands

- `cargo build` / `cargo run`: build and run (serves on `0.0.0.0:8080` by default)
- `cargo check`: quick compilation check
- `cargo test`: 58 unit and integration tests
- `cargo clippy --all-targets -- -D warnings`: lint gate. The crate sets `#![warn(clippy::pedantic)]`, so this is the pedantic bar, and it is clean.
- `cargo fmt`
- `docker compose up -d --build` (or `./deploy.sh`): build the image and run the service

### Configuration

Everything is environment-driven with validated defaults (see `src/config.rs` and `.env.example`): `BIND_ADDRESS`, `PIN_LENGTH`, `STALE_AGE_MINS`, `MAX_PAYLOAD_BYTES`, `MAX_ENTRIES`, `CLEANUP_INTERVAL_SECS`, `REQUEST_TIMEOUT_SECS`, `CORS_ALLOWED_ORIGINS`, `MAX_PINS_PER_NAMESPACE`, `MAX_PROBE_MISSES`, `MAX_GLOBAL_MISSES`, `PROBE_WINDOW_SECS`, `MAX_LONG_POLL_SECS`. Invalid values are rejected at startup rather than silently falling back. `dotenvy` loads `.env` when running outside Docker; logging is `env_logger` via `RUST_LOG`.

## Security notes

- **A PIN is a bearer token and the only access control.** Its width is the entire security story: the default 10 characters over a 32-symbol alphabet is 50 bits, out of reach even unthrottled. `pin.rs` has a test asserting the default stays at or above 48 bits — do not lower it to make PINs prettier. There is no auth.
- **Guess throttling is consulted only after a lookup has already missed.** That ordering is deliberate: it throttles enumeration without letting a guesser lock out the namespace's legitimate user, and legitimate traffic (which barely ever misses) never touches it.
- **Two budgets: per namespace and global.** The global one is the load-bearing half — namespaces are free to invent, so a per-namespace budget alone leaves the total guess rate unbounded. Throttling is defence in depth; `PIN_LENGTH` is the actual defence.
- Both bookkeeping maps are bounded. `namespaces` is capped at `MAX_ENTRIES` and fails *closed* (treating an unknown namespace as throttled) when full, so filling it cannot buy unthrottled guessing; `sweep` prunes idle entries.
- **Never log a namespace or PIN.** The namespace is the only access control this service has, and logs are read by more people than memory is.
- Payload size is enforced by `DefaultBodyLimit` before the body is buffered, so an oversized request is never parsed.

## Testing

- **Unit tests** live beside their modules: config validation, PIN generation and validation (including alphabet coverage and case normalisation), store transitions, error status mapping.
- **Concurrency tests** in `store.rs` cover the properties that matter: a payload reaches exactly one of 32 racing pollers, 64 concurrent allocations never collide, and a write racing a poll is never lost.
- **Integration tests** in `tests/api.rs` drive the real router: full exchange, namespace isolation, delimiter rejection, probe-does-not-allocate, duplicate-PUT conflict, wire-byte size limit, capacity exhaustion, per-namespace quota, guess throttling, readiness, long-poll wake-up and clamping, and the input-validation matrix.

Quota accounting is the fiddly part: `live_pins` must be released on delivery *and* on sweep, or a busy namespace slowly locks itself out. There are tests for both paths.

Bug fixes should be test-first: add the failing case, watch it fail for the expected reason, then fix.
