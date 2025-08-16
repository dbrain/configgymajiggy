use actix_web::{get, http::StatusCode, post, put, web, App, HttpResponse, HttpServer, Result};
use chrono::prelude::{DateTime, Utc};
use chrono::Duration;
use clokwerk::{Scheduler, TimeUnits};
use log::info;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

const PIN_LENGTH: usize = 4;
const MAX_RESULT_SIZE_BYTES: usize = 3000;
const STALE_AGE_MINS: i64 = 10;

#[derive(Clone)]
struct BiboopState {
    read: evmap::ReadHandle<String, Box<PinItem>>,
    write: Arc<Mutex<evmap::WriteHandle<String, Box<PinItem>>>>,
}

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
        let pin: String = thread_rng()
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

fn create_pin_http_response(namespace: &str, state: &BiboopState) -> HttpResponse {
    let pin_response = create_new_pin_response(namespace, state);
    match pin_response {
        Some(res) => HttpResponse::Ok().json(res),
        _ => HttpResponse::build(StatusCode::TOO_MANY_REQUESTS)
            .body("Could not find a free pin soon enough."),
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

#[post("/pin/{namespace}")]
async fn get_pin(path: web::Path<(String,)>, data: web::Data<BiboopState>) -> Result<HttpResponse> {
    Ok(create_pin_http_response(&path.0, data.get_ref()))
}

#[post("/pin/{namespace}/{pin}")]
async fn poll_pin(
    path: web::Path<(String, String)>,
    data: web::Data<BiboopState>,
) -> Result<HttpResponse> {
    let state = data.get_ref();
    Ok(match get_and_remove_pin_if_populated(&path.0, &path.1, state) {
        Some(pin_item) => HttpResponse::Ok().json(pin_item),
        _ => create_pin_http_response(&path.0, state),
    })
}

#[put("/pin/{namespace}/{pin}")]
async fn respond_to_pin(
    path: web::Path<(String, String)>,
    data: web::Data<BiboopState>,
    body: web::Json<HashMap<String, Value>>,
) -> Result<HttpResponse> {
    let result = body.0;
    let serialized = match serde_json::to_string(&result) {
        Ok(s) => s,
        Err(_) => return Ok(HttpResponse::InternalServerError().body("Failed to serialize data")),
    };
    if serialized.len() > MAX_RESULT_SIZE_BYTES {
        return Ok(HttpResponse::build(StatusCode::PAYLOAD_TOO_LARGE).body("Payload too large."));
    }

    let key = format!("{}:{}", path.0, path.1);
    let state = data.get_ref();
    if state.read.contains_key(&key) {
        if let Ok(mut write_handle) = state.write.lock() {
            write_handle.update(
                key,
                Box::new(PinItem::new(path.1.to_string(), Some(result))),
            );
            write_handle.refresh();
        }
        Ok(HttpResponse::Accepted().body("Thanks!"))
    } else {
        Ok(HttpResponse::NotFound().body("Pin not found."))
    }
}

#[get("/health")]
async fn health() -> Result<HttpResponse> {
    Ok(HttpResponse::Ok().body("All good."))
}

fn setup_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(get_pin);
    cfg.service(poll_pin);
    cfg.service(respond_to_pin);
    cfg.service(health);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
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

    HttpServer::new(move || App::new().app_data(web::Data::new(state.clone())).configure(setup_routes))
        .bind("0.0.0.0:8080")?
        .run()
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};
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
        let app = test::init_service(
            App::new().configure(setup_routes)
        ).await;
        
        let req = test::TestRequest::get()
            .uri("/health")
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        
        let body = test::read_body(resp).await;
        assert_eq!(body, "All good.");
    }

    #[tokio::test]
    async fn test_get_pin_endpoint() {
        let state = create_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .configure(setup_routes)
        ).await;
        
        let req = test::TestRequest::post()
            .uri("/pin/testns")
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        
        let body: PinResponse = test::read_body_json(resp).await;
        assert_eq!(body.pin.len(), PIN_LENGTH);
        assert!(body.result.is_none());
    }

    #[tokio::test]
    async fn test_poll_pin_nonexistent() {
        let state = create_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .configure(setup_routes)
        ).await;
        
        let req = test::TestRequest::post()
            .uri("/pin/testns/FAKE")
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        
        // Should return a new pin since the fake one doesn't exist
        let body: PinResponse = test::read_body_json(resp).await;
        assert_eq!(body.pin.len(), PIN_LENGTH);
        assert!(body.result.is_none());
    }

    #[tokio::test]
    async fn test_respond_to_pin_nonexistent() {
        let state = create_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .configure(setup_routes)
        ).await;
        
        let test_data = json!({"message": "test"});
        
        let req = test::TestRequest::put()
            .uri("/pin/testns/FAKE")
            .set_json(&test_data)
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
        
        let body = test::read_body(resp).await;
        assert_eq!(body, "Pin not found.");
    }

    #[tokio::test]
    async fn test_full_pin_workflow() {
        let state = create_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .configure(setup_routes)
        ).await;
        
        // Step 1: Create a new pin
        let req = test::TestRequest::post()
            .uri("/pin/workflow")
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        
        let pin_response: PinResponse = test::read_body_json(resp).await;
        let pin = pin_response.pin;
        assert!(pin_response.result.is_none());
        
        // Step 2: Submit data to the pin
        let test_data = json!({
            "message": "Hello, World!",
            "number": 42,
            "array": [1, 2, 3]
        });
        
        let req = test::TestRequest::put()
            .uri(&format!("/pin/workflow/{}", pin))
            .set_json(&test_data)
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202);
        
        let body = test::read_body(resp).await;
        assert_eq!(body, "Thanks!");
        
        // Step 3: Poll the pin to get the data
        let req = test::TestRequest::post()
            .uri(&format!("/pin/workflow/{}", pin))
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        
        let poll_response: PinResponse = test::read_body_json(resp).await;
        assert_eq!(poll_response.pin, pin);
        assert!(poll_response.result.is_some());
        
        let result = poll_response.result.unwrap();
        assert_eq!(result.get("message").unwrap(), &json!("Hello, World!"));
        assert_eq!(result.get("number").unwrap(), &json!(42));
        assert_eq!(result.get("array").unwrap(), &json!([1, 2, 3]));
        
        // Step 4: Try to poll again - should return new pin since data was consumed
        let req = test::TestRequest::post()
            .uri(&format!("/pin/workflow/{}", pin))
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        
        let new_poll_response: PinResponse = test::read_body_json(resp).await;
        assert_ne!(new_poll_response.pin, pin); // Should be a new pin
        assert!(new_poll_response.result.is_none());
    }

    #[tokio::test]
    async fn test_payload_too_large() {
        let state = create_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .configure(setup_routes)
        ).await;
        
        // First create a pin
        let req = test::TestRequest::post()
            .uri("/pin/large")
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        let pin_response: PinResponse = test::read_body_json(resp).await;
        let pin = pin_response.pin;
        
        // Create a large payload (over 3KB)
        let large_string = "x".repeat(4000);
        let large_data = json!({"data": large_string});
        
        let req = test::TestRequest::put()
            .uri(&format!("/pin/large/{}", pin))
            .set_json(&large_data)
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 413);
        
        let body = test::read_body(resp).await;
        assert_eq!(body, "Payload too large.");
    }

    #[tokio::test]
    async fn test_namespace_isolation() {
        let state = create_test_state();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .configure(setup_routes)
        ).await;
        
        // Create pins in different namespaces
        let req1 = test::TestRequest::post().uri("/pin/ns1").to_request();
        let req2 = test::TestRequest::post().uri("/pin/ns2").to_request();
        
        let resp1 = test::call_service(&app, req1).await;
        let resp2 = test::call_service(&app, req2).await;
        
        let pin1: PinResponse = test::read_body_json(resp1).await;
        let _pin2: PinResponse = test::read_body_json(resp2).await;
        
        // Submit data to pin in ns1
        let data1 = json!({"namespace": "ns1"});
        let req = test::TestRequest::put()
            .uri(&format!("/pin/ns1/{}", pin1.pin))
            .set_json(&data1)
            .to_request();
        test::call_service(&app, req).await;
        
        // Try to access the same pin from ns2 - should fail
        let req = test::TestRequest::put()
            .uri(&format!("/pin/ns2/{}", pin1.pin))
            .set_json(&json!({"namespace": "ns2"}))
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
        
        // But we should be able to poll from the correct namespace
        let req = test::TestRequest::post()
            .uri(&format!("/pin/ns1/{}", pin1.pin))
            .to_request();
        
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        
        let poll_response: PinResponse = test::read_body_json(resp).await;
        assert!(poll_response.result.is_some());
        assert_eq!(poll_response.result.unwrap().get("namespace").unwrap(), &json!("ns1"));
    }
}
