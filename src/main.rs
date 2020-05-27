use actix_web::{get, http::StatusCode, post, put, web, App, HttpResponse, HttpServer, Responder};
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
async fn get_pin(path: web::Path<(String,)>, data: web::Data<BiboopState>) -> impl Responder {
    create_pin_http_response(&path.0, data.get_ref())
}

#[post("/pin/{namespace}/{pin}")]
async fn poll_pin(
    path: web::Path<(String, String)>,
    data: web::Data<BiboopState>,
) -> impl Responder {
    let state = data.get_ref();
    match get_and_remove_pin_if_populated(&path.0, &path.1, state) {
        Some(pin_item) => HttpResponse::Ok().json(pin_item),
        _ => create_pin_http_response(&path.0, state),
    }
}

#[put("/pin/{namespace}/{pin}")]
async fn respond_to_pin(
    path: web::Path<(String, String)>,
    data: web::Data<BiboopState>,
    body: web::Json<HashMap<String, Value>>,
) -> impl Responder {
    let result = body.0;
    let bytes = std::mem::size_of_val(&result);
    if bytes > MAX_RESULT_SIZE_BYTES {
        return HttpResponse::build(StatusCode::PAYLOAD_TOO_LARGE).body("Payload too large.");
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
        HttpResponse::Accepted().body("Thanks!")
    } else {
        HttpResponse::NotFound().body("Pin not found.")
    }
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().body("All good.")
}

fn setup_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(get_pin);
    cfg.service(poll_pin);
    cfg.service(respond_to_pin);
    cfg.service(health);
}

#[actix_rt::main]
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

    HttpServer::new(move || App::new().data(state.clone()).configure(setup_routes))
        .bind("0.0.0.0:8080")
        .unwrap()
        .run()
        .await?;

    Ok(())
}
