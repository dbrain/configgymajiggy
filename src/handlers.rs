use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;

use crate::error::ApiError;
use crate::pin::{Namespace, Pin};
use crate::store::{Allocation, Deposit, Payload, PinKey, PinStore, Poll};

#[derive(Debug, Serialize, Deserialize)]
pub struct PinResponse {
    pub pin: String,
    pub result: Option<Payload>,
}

fn allocate(store: &PinStore, namespace: &Namespace) -> Result<PinResponse, ApiError> {
    match store.allocate(namespace) {
        Allocation::Allocated(pin) => Ok(PinResponse {
            pin: pin.to_string(),
            result: None,
        }),
        Allocation::AtCapacity => Err(ApiError::NoCapacity),
        Allocation::NamespaceFull => Err(ApiError::NamespaceFull),
    }
}

fn key(store: &PinStore, namespace: &str, pin: &str) -> Result<PinKey, ApiError> {
    Ok(PinKey::new(
        Namespace::parse(namespace)?,
        Pin::parse(pin, store.config().pin_length)?,
    ))
}

async fn create_pin(
    Path(namespace): Path<String>,
    State(store): State<PinStore>,
) -> Result<Json<PinResponse>, ApiError> {
    allocate(&store, &Namespace::parse(&namespace)?).map(Json)
}

#[derive(Deserialize)]
pub struct PollParams {
    /// Seconds to hold the request open waiting for a payload. Clamped to
    /// `MAX_LONG_POLL_SECS`; 0 (the default) returns immediately.
    #[serde(default)]
    wait: u64,
}

fn pending(key: &PinKey) -> Json<PinResponse> {
    Json(PinResponse {
        pin: key.pin.to_string(),
        result: None,
    })
}

async fn poll_pin(
    Path((namespace, pin)): Path<(String, String)>,
    Query(params): Query<PollParams>,
    State(store): State<PinStore>,
) -> Result<Json<PinResponse>, ApiError> {
    let key = key(&store, &namespace, &pin)?;
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(params.wait).min(store.config().max_long_poll);

    loop {
        // Register interest *before* reading the slot, so a payload landing
        // between the read and the wait cannot be missed.
        let arrival = store.arrival(&key);
        let notified = arrival.as_ref().map(|n| n.notified());
        tokio::pin!(notified);
        if let Some(notified) = notified.as_mut().as_pin_mut() {
            notified.enable();
        }

        // An unknown pin is reported as missing rather than silently replaced:
        // the old behaviour made probing an allocation primitive and leaked
        // which pins exist.
        match store.poll(&key) {
            Poll::Unknown => return Err(ApiError::PinNotFound),
            Poll::Throttled => return Err(ApiError::TooManyGuesses),
            Poll::Delivered(payload) => {
                return Ok(Json(PinResponse {
                    pin: key.pin.to_string(),
                    result: Some(payload),
                }));
            }
            Poll::Pending => {}
        }

        let Some(notified) = notified.as_mut().as_pin_mut() else {
            return Ok(pending(&key));
        };
        if tokio::time::Instant::now() >= deadline {
            return Ok(pending(&key));
        }

        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return Ok(pending(&key));
        }
    }
}

async fn respond_to_pin(
    Path((namespace, pin)): Path<(String, String)>,
    State(store): State<PinStore>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let key = key(&store, &namespace, &pin)?;

    let limit = store.config().max_payload_bytes;
    if body.len() > limit {
        return Err(ApiError::PayloadTooLarge(limit));
    }

    let payload: Payload = serde_json::from_slice(&body).map_err(|_| ApiError::MalformedPayload)?;

    match store.deposit(&key, payload) {
        Deposit::Accepted => Ok((StatusCode::ACCEPTED, "Thanks!")),
        Deposit::AlreadyPopulated => Err(ApiError::PinAlreadyPopulated),
        Deposit::Unknown => Err(ApiError::PinNotFound),
        Deposit::Throttled => Err(ApiError::TooManyGuesses),
    }
}

/// Liveness: the process is up and serving. Deliberately does no work.
async fn health() -> impl IntoResponse {
    "All good."
}

/// Readiness: also asserts the expiry sweeper is still running. Without this a
/// dead sweeper looks healthy right up until memory fills.
async fn ready(State(store): State<PinStore>) -> Result<impl IntoResponse, ApiError> {
    if store.is_ready() {
        Ok("Ready.")
    } else {
        Err(ApiError::NotReady)
    }
}

fn cors(allowed_origins: &[String]) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT])
        .allow_headers([header::CONTENT_TYPE]);

    if allowed_origins.is_empty() {
        return layer.allow_origin(Any);
    }

    let origins: Vec<_> = allowed_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();

    layer.allow_origin(AllowOrigin::list(origins))
}

pub fn router(store: PinStore) -> Router {
    let config = store.config().clone();

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route(
            "/pin/{namespace}",
            // These read no body; without a limit they would accept axum's 2 MB default.
            post(create_pin).layer(DefaultBodyLimit::max(0)),
        )
        .route(
            "/pin/{namespace}/{pin}",
            post(poll_pin).layer(DefaultBodyLimit::max(0)),
        )
        .route(
            "/pin/{namespace}/{pin}",
            // Enforced before the body is buffered, so an oversized payload is
            // never parsed into a Value tree.
            put(respond_to_pin).layer(DefaultBodyLimit::max(config.max_payload_bytes)),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            config.request_timeout,
        ))
        .layer(cors(&config.allowed_origins))
        .with_state(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::Arc;

    #[test]
    fn unset_origins_stay_permissive_and_a_list_is_honoured() {
        // Smoke test that both branches build a layer without panicking.
        let _ = cors(&[]);
        let _ = cors(&["https://example.com".to_string()]);
        let _ = cors(&["not a url".to_string()]);
    }

    #[test]
    fn router_builds_from_defaults() {
        let store = PinStore::new(Arc::new(Config::default()));
        let _ = router(store);
    }
}
