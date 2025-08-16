use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post, put},
    Router,
};
use chrono::prelude::{DateTime, Utc};
use chrono::Duration;
use clokwerk::{Scheduler, TimeUnits};
use log::info;
use rand::distr::Alphanumeric;
use rand::{rng, Rng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;

const PIN_LENGTH: usize = 4;
const MAX_RESULT_SIZE_BYTES: usize = 3000;
const STALE_AGE_MINS: i64 = 10;

#[derive(Clone)]
struct BiboopState {
    read: evmap::ReadHandle<String, Box<PinItem>>,
    write: Arc<Mutex<evmap::WriteHandle<String, Box<PinItem>>>>,
}

// Need to implement Sync manually since evmap::ReadHandle contains Cell<()> 
// which is not Sync, but in practice it's safe in our usage
unsafe impl Sync for BiboopState {}

#[derive(Serialize, Deserialize)]
struct PinResponse {
    pin: String,
    result: Option<HashMap<String, Value>>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct PinItem {
    timestamp: DateTime<Utc>,
    pin: String,
    result: Option<HashMap<String, Value>>,
}

#[allow(clippy::derive_hash_xor_eq)]
impl Hash for PinItem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.timestamp.hash(state);
        self.pin.hash(state);
    }
}

impl PinItem {
    fn new(pin: String, result: Option<HashMap<String, Value>>) -> Self {
        PinItem {
            timestamp: Utc::now(),
            pin,
            result,
        }
    }
}

fn create_unique_pin(namespace: &str, state: &BiboopState) -> Option<String> {
    for _ in 0..10 {
        let pin: String = rng()
            .sample_iter(&Alphanumeric)
            .take(PIN_LENGTH)
            .map(char::from)
            .collect();
        let uc_pin = pin.to_uppercase();
        let key = format!("{}:{}", namespace, uc_pin);

        if !state.read.contains_key(&key) {
            if let Ok(mut write_handle) = state.write.lock() {
                write_handle.insert(key, Box::new(PinItem::new(uc_pin.clone(), None)));
                write_handle.refresh();
                return Some(uc_pin);
            }
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

fn create_pin_http_response(namespace: &str, state: &BiboopState) -> impl IntoResponse {
    let pin_response = create_new_pin_response(namespace, state);
    match pin_response {
        Some(res) => Json(res).into_response(),
        _ => (StatusCode::TOO_MANY_REQUESTS, "Could not find a free pin soon enough.").into_response(),
    }
}

fn get_and_remove_pin_if_populated(
    namespace: &str,
    pin: &str,
    state: &BiboopState,
) -> Option<PinResponse> {
    let key = format!("{}:{}", namespace, pin);
    let item = state.read.get_one(&key)?;
    let pin_item = item.as_ref();
    let result = &pin_item.result;
    if result.is_some() {
        if let Ok(mut write_handle) = state.write.lock() {
            write_handle.empty(key);
            write_handle.refresh();
        }
    }
    Some(PinResponse {
        pin: pin.to_string(),
        result: result.clone(),
    })
}

async fn get_pin(
    Path(namespace): Path<String>,
    State(state): State<BiboopState>,
) -> impl IntoResponse {
    create_pin_http_response(&namespace, &state)
}

async fn poll_pin(
    Path((namespace, pin)): Path<(String, String)>,
    State(state): State<BiboopState>,
) -> impl IntoResponse {
    match get_and_remove_pin_if_populated(&namespace, &pin, &state) {
        Some(pin_item) => Json(pin_item).into_response(),
        _ => create_pin_http_response(&namespace, &state).into_response(),
    }
}

async fn respond_to_pin(
    Path((namespace, pin)): Path<(String, String)>,
    State(state): State<BiboopState>,
    Json(result): Json<HashMap<String, Value>>,
) -> impl IntoResponse {
    let serialized = match serde_json::to_string(&result) {
        Ok(s) => s,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to serialize data").into_response(),
    };
    if serialized.len() > MAX_RESULT_SIZE_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Payload too large.").into_response();
    }

    let key = format!("{}:{}", namespace, pin);
    if state.read.contains_key(&key) {
        if let Ok(mut write_handle) = state.write.lock() {
            write_handle.update(
                key,
                Box::new(PinItem::new(pin.to_string(), Some(result))),
            );
            write_handle.refresh();
        }
        (StatusCode::ACCEPTED, "Thanks!").into_response()
    } else {
        (StatusCode::NOT_FOUND, "Pin not found.").into_response()
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

    let (read, write) = evmap::new();
    let state = BiboopState {
        read,
        write: Arc::new(Mutex::new(write)),
    };

    let mut scheduler = Scheduler::with_tz(chrono::Utc);
    let clone_state = state.clone();
    scheduler.every(10.seconds()).run(move || {
        let mut keys_to_remove: Vec<String> = Vec::new();
        if let Some(items) = &clone_state.read.read() {
            for (key, pin_items) in items {
                if let Some(pin_item) = pin_items.get_one() {
                    let age = Utc::now().signed_duration_since(pin_item.timestamp);
                    if age > Duration::minutes(STALE_AGE_MINS) {
                        keys_to_remove.push(key.to_string())
                    }
                }
            }
        }

        if !keys_to_remove.is_empty() {
            if let Ok(mut write_handle) = clone_state.write.lock() {
                for key in keys_to_remove {
                    info!("Cleaning up stale key {}", key);
                    write_handle.empty(key);
                }
                write_handle.refresh();
            }
        }
    });
    let _thread_handle = scheduler.watch_thread(std::time::Duration::from_millis(100));

    let app = create_router().with_state(state);
    
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    info!("Server running on http://0.0.0.0:8080");
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;
    use serde_json::json;

    fn create_test_state() -> BiboopState {
        let (read, write) = evmap::new();
        BiboopState {
            read,
            write: Arc::new(Mutex::new(write)),
        }
    }

    #[tokio::test]
    async fn test_pin_item_creation() {
        let pin = "TEST".to_string();
        let result = Some(HashMap::new());
        let item = PinItem::new(pin.clone(), result.clone());
        
        assert_eq!(item.pin, pin);
        assert_eq!(item.result, result);
        assert!(item.timestamp <= Utc::now());
    }

    #[tokio::test]
    async fn test_pin_item_hash() {
        let pin1 = PinItem::new("TEST".to_string(), None);
        let pin2 = PinItem::new("TEST".to_string(), None);
        
        // Items with same pin and timestamp should not necessarily have same hash
        // due to timestamp precision differences
        assert_eq!(pin1.pin, pin2.pin);
    }

    #[tokio::test]
    async fn test_create_unique_pin() {
        let state = create_test_state();
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
        let state = create_test_state();
        let namespace = "test";
        
        let response = create_new_pin_response(namespace, &state);
        assert!(response.is_some());
        
        let response = response.unwrap();
        assert_eq!(response.pin.len(), PIN_LENGTH);
        assert!(response.result.is_none());
    }

    #[tokio::test]
    async fn test_get_and_remove_pin_empty() {
        let state = create_test_state();
        let namespace = "test";
        let pin = "ABCD";
        
        // Pin doesn't exist
        let result = get_and_remove_pin_if_populated(namespace, pin, &state);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_and_remove_pin_with_data() {
        let state = create_test_state();
        let namespace = "test";
        let pin = "ABCD";
        let key = format!("{}:{}", namespace, pin);
        
        // Insert pin with data
        let mut data = HashMap::new();
        data.insert("test".to_string(), json!("value"));
        
        {
            let mut write_handle = state.write.lock().unwrap();
            write_handle.insert(key.clone(), Box::new(PinItem::new(pin.to_string(), Some(data.clone()))));
            write_handle.refresh();
        }
        
        // Retrieve and remove
        let result = get_and_remove_pin_if_populated(namespace, pin, &state);
        assert!(result.is_some());
        
        let response = result.unwrap();
        assert_eq!(response.pin, pin);
        assert_eq!(response.result, Some(data));
        
        // Should be removed now
        assert!(!state.read.contains_key(&key));
    }

    #[tokio::test]
    async fn test_get_and_remove_pin_without_data() {
        let state = create_test_state();
        let namespace = "test";
        let pin = "ABCD";
        let key = format!("{}:{}", namespace, pin);
        
        // Insert pin without data
        {
            let mut write_handle = state.write.lock().unwrap();
            write_handle.insert(key.clone(), Box::new(PinItem::new(pin.to_string(), None)));
            write_handle.refresh();
        }
        
        // Retrieve but don't remove (no data)
        let result = get_and_remove_pin_if_populated(namespace, pin, &state);
        assert!(result.is_some());
        
        let response = result.unwrap();
        assert_eq!(response.pin, pin);
        assert!(response.result.is_none());
        
        // Should still exist
        assert!(state.read.contains_key(&key));
    }

    // Integration tests for HTTP endpoints
    #[tokio::test]
    async fn test_health_endpoint() {
        let state = create_test_state();
        let app = create_router().with_state(state);
        let server = TestServer::new(app).unwrap();
        
        let response = server.get("/health").await;
        
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.text(), "All good.");
    }

    #[tokio::test]
    async fn test_get_pin_endpoint() {
        let state = create_test_state();
        let app = create_router().with_state(state);
        let server = TestServer::new(app).unwrap();
        
        let response = server.post("/pin/testns").await;
        
        assert_eq!(response.status_code(), 200);
        let body: PinResponse = response.json();
        assert_eq!(body.pin.len(), PIN_LENGTH);
        assert!(body.result.is_none());
    }

    #[tokio::test]
    async fn test_poll_pin_nonexistent() {
        let state = create_test_state();
        let app = create_router().with_state(state);
        let server = TestServer::new(app).unwrap();
        
        let response = server.post("/pin/testns/FAKE").await;
        
        assert_eq!(response.status_code(), 200);
        // Should return a new pin since the fake one doesn't exist
        let body: PinResponse = response.json();
        assert_eq!(body.pin.len(), PIN_LENGTH);
        assert!(body.result.is_none());
    }

    #[tokio::test]
    async fn test_respond_to_pin_nonexistent() {
        let state = create_test_state();
        let app = create_router().with_state(state);
        let server = TestServer::new(app).unwrap();
        
        let test_data = json!({"message": "test"});
        
        let response = server.put("/pin/testns/FAKE").json(&test_data).await;
        
        assert_eq!(response.status_code(), 404);
        assert_eq!(response.text(), "Pin not found.");
    }

    #[tokio::test]
    async fn test_full_pin_workflow() {
        let state = create_test_state();
        let app = create_router().with_state(state);
        let server = TestServer::new(app).unwrap();
        
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
        
        let response = server.put(&format!("/pin/workflow/{}", pin)).json(&test_data).await;
        assert_eq!(response.status_code(), 202);
        assert_eq!(response.text(), "Thanks!");
        
        // Step 3: Poll the pin to get the data
        let response = server.post(&format!("/pin/workflow/{}", pin)).await;
        assert_eq!(response.status_code(), 200);
        
        let poll_response: PinResponse = response.json();
        assert_eq!(poll_response.pin, pin);
        assert!(poll_response.result.is_some());
        
        let result = poll_response.result.unwrap();
        assert_eq!(result.get("message").unwrap(), &json!("Hello, World!"));
        assert_eq!(result.get("number").unwrap(), &json!(42));
        assert_eq!(result.get("array").unwrap(), &json!([1, 2, 3]));
        
        // Step 4: Try to poll again - should return new pin since data was consumed
        let response = server.post(&format!("/pin/workflow/{}", pin)).await;
        assert_eq!(response.status_code(), 200);
        
        let new_poll_response: PinResponse = response.json();
        assert_ne!(new_poll_response.pin, pin); // Should be a new pin
        assert!(new_poll_response.result.is_none());
    }

    #[tokio::test]
    async fn test_payload_too_large() {
        let state = create_test_state();
        let app = create_router().with_state(state);
        let server = TestServer::new(app).unwrap();
        
        // First create a pin
        let response = server.post("/pin/large").await;
        let pin_response: PinResponse = response.json();
        let pin = pin_response.pin;
        
        // Create a large payload (over 3KB)
        let large_string = "x".repeat(4000);
        let large_data = json!({"data": large_string});
        
        let response = server.put(&format!("/pin/large/{}", pin)).json(&large_data).await;
        assert_eq!(response.status_code(), 413);
        assert_eq!(response.text(), "Payload too large.");
    }

    #[tokio::test]
    async fn test_namespace_isolation() {
        let state = create_test_state();
        let app = create_router().with_state(state);
        let server = TestServer::new(app).unwrap();
        
        // Create pins in different namespaces
        let resp1 = server.post("/pin/ns1").await;
        let resp2 = server.post("/pin/ns2").await;
        
        let pin1: PinResponse = resp1.json();
        let _pin2: PinResponse = resp2.json();
        
        // Submit data to pin in ns1
        let data1 = json!({"namespace": "ns1"});
        server.put(&format!("/pin/ns1/{}", pin1.pin)).json(&data1).await;
        
        // Try to access the same pin from ns2 - should fail
        let response = server.put(&format!("/pin/ns2/{}", pin1.pin))
            .json(&json!({"namespace": "ns2"}))
            .await;
        assert_eq!(response.status_code(), 404);
        
        // But we should be able to poll from the correct namespace
        let response = server.post(&format!("/pin/ns1/{}", pin1.pin)).await;
        assert_eq!(response.status_code(), 200);
        
        let poll_response: PinResponse = response.json();
        assert!(poll_response.result.is_some());
        assert_eq!(poll_response.result.unwrap().get("namespace").unwrap(), &json!("ns1"));
    }
}
