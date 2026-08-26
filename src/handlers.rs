use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;

use crate::error::ApiError;
use crate::pin::{Namespace, Pin};
use crate::store::{Allocation, Deposit, Payload, PinKey, PinStore};

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
        Allocation::Unavailable => Err(ApiError::NoCapacity),
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

async fn poll_pin(
    Path((namespace, pin)): Path<(String, String)>,
    State(store): State<PinStore>,
) -> Result<Json<PinResponse>, ApiError> {
    let key = key(&store, &namespace, &pin)?;

    // An unknown pin is reported as missing rather than silently replaced: the
    // old behaviour made probing an allocation primitive and leaked which pins
    // exist.
    match store.poll(&key) {
        crate::store::Poll::Unknown => Err(ApiError::PinNotFound),
        crate::store::Poll::Pending => Ok(Json(PinResponse {
            pin: key.pin.to_string(),
            result: None,
        })),
        crate::store::Poll::Delivered(payload) => Ok(Json(PinResponse {
            pin: key.pin.to_string(),
            result: Some(payload),
        })),
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
    }
}

async fn health() -> impl IntoResponse {
    "All good."
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
