use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::{get, post, put},
};
use chrono::TimeDelta;
use chrono::prelude::{DateTime, Utc};
use dashmap::{DashMap, mapref::entry::Entry};
use log::info;
use rand::distr::Alphanumeric;
use rand::{RngExt, rng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

const PIN_LENGTH: usize = 4;
const MAX_RESULT_SIZE_BYTES: usize = 3000;
const STALE_AGE_MINS: i64 = 10;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct BiboopState {
    pins: Arc<DashMap<String, PinItem>>,
}

impl BiboopState {
    fn new() -> Self {
        BiboopState {
            pins: Arc::new(DashMap::new()),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PinResponse {
    pin: String,
    result: Option<HashMap<String, Value>>,
}

#[derive(Clone)]
struct PinItem {
    timestamp: DateTime<Utc>,
    result: Option<HashMap<String, Value>>,
}

impl PinItem {
    fn new(result: Option<HashMap<String, Value>>) -> Self {
        PinItem {
            timestamp: Utc::now(),
            result,
        }
    }
}

fn create_key(namespace: &str, pin: &str) -> String {
    format!("{namespace}:{pin}")
}

fn create_unique_pin(namespace: &str, state: &BiboopState) -> Option<String> {
    for _ in 0..10 {
        let pin: String = rng()
            .sample_iter(&Alphanumeric)
            .take(PIN_LENGTH)
            .map(char::from)
            .collect::<String>()
            .to_uppercase();

        if let Entry::Vacant(slot) = state.pins.entry(create_key(namespace, &pin)) {
            slot.insert(PinItem::new(None));
            return Some(pin);
        }
    }
    None
}

fn create_new_pin_response(namespace: &str, state: &BiboopState) -> Option<PinResponse> {
    let unique_pin = create_unique_pin(namespace, state)?;
    Some(PinResponse {
        pin: unique_pin,
        result: None,
    })
}

fn create_pin_http_response(namespace: &str, state: &BiboopState) -> Response {
    match create_new_pin_response(namespace, state) {
        Some(res) => Json(res).into_response(),
        None => (
            StatusCode::TOO_MANY_REQUESTS,
            "Could not find a free pin soon enough.",
        )
            .into_response(),
    }
}

fn get_and_remove_pin_if_populated(
    namespace: &str,
    pin: &str,
    state: &BiboopState,
) -> Option<PinResponse> {
    let key = create_key(namespace, pin);

    // remove_if keeps the read and the take under one shard lock, so two
    // concurrent polls can never both consume the same result.
    if let Some((_, item)) = state.pins.remove_if(&key, |_, item| item.result.is_some()) {
        return Some(PinResponse {
            pin: pin.to_string(),
            result: item.result,
        });
    }

    state.pins.contains_key(&key).then(|| PinResponse {
        pin: pin.to_string(),
        result: None,
    })
}

fn remove_stale_pins(state: &BiboopState) {
    let cutoff = Utc::now() - TimeDelta::minutes(STALE_AGE_MINS);
    state.pins.retain(|key, item| {
        let fresh = item.timestamp > cutoff;
        if !fresh {
            info!("Cleaning up stale key {key}");
        }
        fresh
    });
}

async fn get_pin(Path(namespace): Path<String>, State(state): State<BiboopState>) -> Response {
    create_pin_http_response(&namespace, &state)
}

async fn poll_pin(
    Path((namespace, pin)): Path<(String, String)>,
    State(state): State<BiboopState>,
) -> Response {
    match get_and_remove_pin_if_populated(&namespace, &pin, &state) {
        Some(pin_item) => Json(pin_item).into_response(),
        None => create_pin_http_response(&namespace, &state),
    }
}

async fn respond_to_pin(
    Path((namespace, pin)): Path<(String, String)>,
    State(state): State<BiboopState>,
    Json(result): Json<HashMap<String, Value>>,
) -> Response {
    let serialized = match serde_json::to_string(&result) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to serialize data",
            )
                .into_response();
        }
    };
    if serialized.len() > MAX_RESULT_SIZE_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Payload too large.").into_response();
    }

    match state.pins.get_mut(&create_key(&namespace, &pin)) {
        Some(mut item) => {
            *item = PinItem::new(Some(result));
            (StatusCode::ACCEPTED, "Thanks!").into_response()
        }
        None => (StatusCode::NOT_FOUND, "Pin not found.").into_response(),
    }
}

async fn health() -> impl IntoResponse {
    "All good."
}

fn create_router() -> Router<BiboopState> {
    Router::new()
        .route("/health", get(health))
        .route("/pin/{namespace}", post(get_pin))
        .route("/pin/{namespace}/{pin}", post(poll_pin))
        .route("/pin/{namespace}/{pin}", put(respond_to_pin))
        .layer(CorsLayer::permissive())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    env_logger::init();

    let state = BiboopState::new();

    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
        loop {
            ticker.tick().await;
            remove_stale_pins(&cleanup_state);
        }
    });

    let app = create_router().with_state(state);

    let bind_addr = std::env::var("BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!("Server running on http://{bind_addr}");
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;
    use serde_json::json;

    fn test_server_with(state: BiboopState) -> TestServer {
        TestServer::new(create_router().with_state(state))
    }

    fn test_server() -> TestServer {
        test_server_with(BiboopState::new())
    }

    fn sample_data() -> HashMap<String, Value> {
        HashMap::from([("test".to_string(), json!("value"))])
    }

    #[tokio::test]
    async fn test_pin_item_creation() {
        let result = Some(sample_data());
        let item = PinItem::new(result.clone());

        assert_eq!(item.result, result);
        assert!(item.timestamp <= Utc::now());
    }

    #[tokio::test]
    async fn test_create_unique_pin() {
        let state = BiboopState::new();
        let namespace = "test";

        let pin1 = create_unique_pin(namespace, &state);
        assert!(pin1.is_some());

        let pin1_val = pin1.unwrap();
        assert_eq!(pin1_val.len(), PIN_LENGTH);

        // Second pin should be different
        let pin2 = create_unique_pin(namespace, &state);
        assert!(pin2.is_some());
        let pin2_val = pin2.unwrap();
        assert_ne!(pin1_val, pin2_val);
    }

    #[tokio::test]
    async fn test_create_new_pin_response() {
        let state = BiboopState::new();

        let response = create_new_pin_response("test", &state);
        assert!(response.is_some());

        let response = response.unwrap();
        assert_eq!(response.pin.len(), PIN_LENGTH);
        assert!(response.result.is_none());
    }

    #[tokio::test]
    async fn test_get_and_remove_pin_empty() {
        let state = BiboopState::new();

        // Pin doesn't exist
        let result = get_and_remove_pin_if_populated("test", "ABCD", &state);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_and_remove_pin_with_data() {
        let state = BiboopState::new();
        let namespace = "test";
        let pin = "ABCD";
        let key = create_key(namespace, pin);
        let data = sample_data();

        state
            .pins
            .insert(key.clone(), PinItem::new(Some(data.clone())));

        let result = get_and_remove_pin_if_populated(namespace, pin, &state);
        assert!(result.is_some());

        let response = result.unwrap();
        assert_eq!(response.pin, pin);
        assert_eq!(response.result, Some(data));

        // Should be removed now
        assert!(!state.pins.contains_key(&key));
    }

    #[tokio::test]
    async fn test_get_and_remove_pin_without_data() {
        let state = BiboopState::new();
        let namespace = "test";
        let pin = "ABCD";
        let key = create_key(namespace, pin);

        state.pins.insert(key.clone(), PinItem::new(None));

        // Retrieve but don't remove (no data)
        let result = get_and_remove_pin_if_populated(namespace, pin, &state);
        assert!(result.is_some());

        let response = result.unwrap();
        assert_eq!(response.pin, pin);
        assert!(response.result.is_none());

        // Should still exist
        assert!(state.pins.contains_key(&key));
    }

    #[tokio::test]
    async fn test_remove_stale_pins() {
        let state = BiboopState::new();

        let mut stale = PinItem::new(None);
        stale.timestamp = Utc::now() - TimeDelta::minutes(STALE_AGE_MINS + 1);
        state.pins.insert(create_key("ns", "OLD1"), stale);

        let mut borderline = PinItem::new(Some(sample_data()));
        borderline.timestamp = Utc::now() - TimeDelta::minutes(STALE_AGE_MINS - 1);
        state.pins.insert(create_key("ns", "OLD2"), borderline);

        state
            .pins
            .insert(create_key("ns", "NEW1"), PinItem::new(None));

        remove_stale_pins(&state);

        assert!(!state.pins.contains_key(&create_key("ns", "OLD1")));
        assert!(state.pins.contains_key(&create_key("ns", "OLD2")));
        assert!(state.pins.contains_key(&create_key("ns", "NEW1")));
    }

    // Integration tests for HTTP endpoints
    #[tokio::test]
    async fn test_health_endpoint() {
        let response = test_server().get("/health").await;

        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "All good.");
    }

    #[tokio::test]
    async fn test_get_pin_endpoint() {
        let response = test_server().post("/pin/testns").await;

        assert_eq!(response.status_code(), 200);
        let body: PinResponse = response.json();
        assert_eq!(body.pin.len(), PIN_LENGTH);
        assert!(body.result.is_none());
    }

    #[tokio::test]
    async fn test_poll_pin_nonexistent() {
        let response = test_server().post("/pin/testns/FAKE").await;

        assert_eq!(response.status_code(), 200);
        // Should return a new pin since the fake one doesn't exist
        let body: PinResponse = response.json();
        assert_eq!(body.pin.len(), PIN_LENGTH);
        assert!(body.result.is_none());
    }

    #[tokio::test]
    async fn test_respond_to_pin_nonexistent() {
        let response = test_server()
            .put("/pin/testns/FAKE")
            .json(&json!({"message": "test"}))
            .await;

        assert_eq!(response.status_code(), 404);
        assert_eq!(response.text(), "Pin not found.");
    }

    #[tokio::test]
    async fn test_full_pin_workflow() {
        let server = test_server();

        // Step 1: Create a new pin
        let response = server.post("/pin/workflow").await;
        assert_eq!(response.status_code(), 200);

        let pin_response: PinResponse = response.json();
        let pin = pin_response.pin;
        assert!(pin_response.result.is_none());

        // Step 2: Submit data to the pin
        let test_data = json!({
            "message": "Hello, World!",
            "number": 42,
            "array": [1, 2, 3]
        });

        let response = server
            .put(&format!("/pin/workflow/{pin}"))
            .json(&test_data)
            .await;
        assert_eq!(response.status_code(), 202);
        assert_eq!(response.text(), "Thanks!");

        // Step 3: Poll the pin to get the data
        let response = server.post(&format!("/pin/workflow/{pin}")).await;
        assert_eq!(response.status_code(), 200);

        let poll_response: PinResponse = response.json();
        assert_eq!(poll_response.pin, pin);

        let result = poll_response.result.unwrap();
        assert_eq!(result.get("message").unwrap(), &json!("Hello, World!"));
        assert_eq!(result.get("number").unwrap(), &json!(42));
        assert_eq!(result.get("array").unwrap(), &json!([1, 2, 3]));

        // Step 4: Try to poll again - should return new pin since data was consumed
        let response = server.post(&format!("/pin/workflow/{pin}")).await;
        assert_eq!(response.status_code(), 200);

        let new_poll_response: PinResponse = response.json();
        assert_ne!(new_poll_response.pin, pin); // Should be a new pin
        assert!(new_poll_response.result.is_none());
    }

    #[tokio::test]
    async fn test_payload_too_large() {
        let server = test_server();

        let response = server.post("/pin/large").await;
        let pin_response: PinResponse = response.json();
        let pin = pin_response.pin;

        let large_data = json!({ "data": "x".repeat(MAX_RESULT_SIZE_BYTES + 1000) });

        let response = server
            .put(&format!("/pin/large/{pin}"))
            .json(&large_data)
            .await;
        assert_eq!(response.status_code(), 413);
        assert_eq!(response.text(), "Payload too large.");
    }

    #[tokio::test]
    async fn test_namespace_isolation() {
        let server = test_server();

        // Create pins in different namespaces
        let pin1: PinResponse = server.post("/pin/ns1").await.json();
        let _pin2: PinResponse = server.post("/pin/ns2").await.json();

        // Submit data to pin in ns1
        server
            .put(&format!("/pin/ns1/{}", pin1.pin))
            .json(&json!({"namespace": "ns1"}))
            .await;

        // Try to access the same pin from ns2 - should fail
        let response = server
            .put(&format!("/pin/ns2/{}", pin1.pin))
            .json(&json!({"namespace": "ns2"}))
            .await;
        assert_eq!(response.status_code(), 404);

        // But we should be able to poll from the correct namespace
        let response = server.post(&format!("/pin/ns1/{}", pin1.pin)).await;
        assert_eq!(response.status_code(), 200);

        let poll_response: PinResponse = response.json();
        assert_eq!(
            poll_response.result.unwrap().get("namespace").unwrap(),
            &json!("ns1")
        );
    }

    #[tokio::test]
    async fn test_concurrent_pin_creation() {
        let state = BiboopState::new();

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let state = state.clone();
                tokio::spawn(async move { create_unique_pin("concurrent", &state) })
            })
            .collect();

        let mut pins = Vec::new();
        for handle in handles {
            if let Some(pin) = handle.await.unwrap() {
                pins.push(pin);
            }
        }

        assert_eq!(pins.len(), 10, "All PIN creations should succeed");
        pins.sort();
        pins.dedup();
        assert_eq!(pins.len(), 10, "All PINs should be unique");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_result_consumed_exactly_once() {
        let state = BiboopState::new();
        let namespace = "once";
        let pin = create_unique_pin(namespace, &state).unwrap();
        state.pins.insert(
            create_key(namespace, &pin),
            PinItem::new(Some(sample_data())),
        );

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let state = state.clone();
                let (namespace, pin) = (namespace.to_string(), pin.clone());
                tokio::spawn(async move {
                    get_and_remove_pin_if_populated(&namespace, &pin, &state)
                        .and_then(|response| response.result)
                })
            })
            .collect();

        let mut consumed = 0;
        for handle in handles {
            if handle.await.unwrap().is_some() {
                consumed += 1;
            }
        }

        assert_eq!(
            consumed, 1,
            "Result must be handed out to exactly one poller"
        );
    }

    #[tokio::test]
    async fn test_high_frequency_operations() {
        let server = test_server();
        let start = std::time::Instant::now();

        for i in 0..100 {
            let namespace = format!("perf_{}", i % 10);

            let response = server.post(&format!("/pin/{namespace}")).await;
            assert_eq!(response.status_code(), 200);
            let pin_response: PinResponse = response.json();
            let pin = pin_response.pin;

            let response = server
                .put(&format!("/pin/{namespace}/{pin}"))
                .json(&json!({"iteration": i, "data": "performance_test"}))
                .await;
            assert_eq!(response.status_code(), 202);

            let response = server.post(&format!("/pin/{namespace}/{pin}")).await;
            assert_eq!(response.status_code(), 200);
            let poll_response: PinResponse = response.json();
            assert!(poll_response.result.is_some());
        }

        let duration = start.elapsed();
        println!("100 complete PIN operations took: {duration:?}");
        assert!(
            duration.as_secs() < 5,
            "Operations should complete in under 5 seconds"
        );
    }

    #[tokio::test]
    async fn test_memory_usage_under_load() {
        let state = BiboopState::new();

        let mut pins = Vec::new();
        for i in 0..1000 {
            let namespace = format!("memory_{}", i % 50);
            if let Some(pin) = create_unique_pin(&namespace, &state) {
                pins.push((namespace, pin));
            }
        }

        assert!(pins.len() >= 950, "Should be able to create most PINs");

        for (namespace, pin) in &pins[..100] {
            assert!(
                state.pins.contains_key(&create_key(namespace, pin)),
                "PIN should exist in map"
            );
        }
    }

    #[tokio::test]
    async fn test_namespace_scaling() {
        let state = BiboopState::new();

        let handles: Vec<_> = (0..50)
            .map(|i| {
                let state = state.clone();
                tokio::spawn(async move {
                    let namespace = format!("scale_ns_{i}");
                    let pin = create_unique_pin(&namespace, &state).unwrap();

                    let data =
                        HashMap::from([("namespace_id".to_string(), Value::Number(i.into()))]);
                    state
                        .pins
                        .insert(create_key(&namespace, &pin), PinItem::new(Some(data)));

                    let retrieved = get_and_remove_pin_if_populated(&namespace, &pin, &state);
                    retrieved
                        .unwrap()
                        .result
                        .unwrap()
                        .get("namespace_id")
                        .unwrap()
                        .as_i64()
                        .unwrap()
                })
            })
            .collect();

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        results.sort();
        assert_eq!(results, (0..50).collect::<Vec<i64>>());
    }
}
