use axum_test::TestServer;
use configgymajiggy::{Config, PinResponse, PinStore, router};
use serde_json::{Value, json};
use std::sync::Arc;

fn server_with(config: Config) -> (TestServer, PinStore) {
    let store = PinStore::new(Arc::new(config));
    (TestServer::new(router(store.clone())), store)
}

fn server() -> (TestServer, PinStore) {
    server_with(Config::default())
}

async fn allocate(server: &TestServer, namespace: &str) -> String {
    let response = server.post(&format!("/pin/{namespace}")).await;
    assert_eq!(response.status_code(), 200, "allocation should succeed");
    response.json::<PinResponse>().pin
}

#[tokio::test]
async fn namespace_containing_delimiter_is_rejected() {
    let (server, _store) = server();

    assert_eq!(server.post("/pin/tenant:eu").await.status_code(), 400);
}

#[tokio::test]
async fn structured_namespace_cannot_be_aliased_from_its_parent() {
    let (server, _store) = server();

    // A tenant using a structured namespace. ("acme:eu", PIN) and ("acme", "eu:PIN")
    // both flatten to the key "acme:eu:PIN" under delimiter-joined keys.
    let allocated = server.post("/pin/acme:eu").await;
    if allocated.status_code() == 400 {
        return; // structured namespaces are refused outright, so no alias exists
    }
    let pin = allocated.json::<PinResponse>().pin;

    let attacker = server
        .put(&format!("/pin/acme/eu:{pin}"))
        .json(&json!({"stolen": true}))
        .await;
    assert_ne!(
        attacker.status_code(),
        202,
        "a write from namespace 'acme' was accepted into namespace 'acme:eu'"
    );

    let delivered: PinResponse = server.post(&format!("/pin/acme:eu/{pin}")).await.json();
    assert!(
        delivered.result.is_none(),
        "a foreign namespace's payload was delivered to this pin: {:?}",
        delivered.result
    );
}

#[tokio::test]
async fn polling_an_unknown_pin_is_not_found_and_allocates_nothing() {
    let (server, store) = server();

    let response = server.post("/pin/probe/ZZZZ").await;
    assert_eq!(response.status_code(), 404);
    assert_eq!(
        store.len(),
        0,
        "probing an unknown pin must not allocate an entry"
    );
}

#[tokio::test]
async fn probing_cannot_grow_the_map() {
    let (server, store) = server();

    for i in 0..50 {
        server.post(&format!("/pin/probe/Q{i:03}")).await;
    }

    assert_eq!(
        store.len(),
        0,
        "50 probes allocated {} entries",
        store.len()
    );
}

#[tokio::test]
async fn second_submission_does_not_clobber_an_undelivered_payload() {
    let (server, _store) = server();
    let pin = allocate(&server, "clobber").await;

    let first = server
        .put(&format!("/pin/clobber/{pin}"))
        .json(&json!({"sender": "first"}))
        .await;
    assert_eq!(first.status_code(), 202);

    let second = server
        .put(&format!("/pin/clobber/{pin}"))
        .json(&json!({"sender": "second"}))
        .await;
    assert_eq!(
        second.status_code(),
        409,
        "a second write to a populated pin must be refused"
    );

    let delivered: PinResponse = server.post(&format!("/pin/clobber/{pin}")).await.json();
    assert_eq!(
        delivered.result.unwrap().get("sender").unwrap(),
        &json!("first"),
        "the first sender's payload must survive"
    );
}

#[tokio::test]
async fn oversized_body_is_rejected_on_wire_bytes_not_compact_json() {
    let (server, _store) = server();
    let pin = allocate(&server, "whitespace").await;

    // Compact form is tiny; the wire body is far over the limit.
    let padded = format!("{{\"a\":{}1}}", " ".repeat(64 * 1024));

    let response = server
        .put(&format!("/pin/whitespace/{pin}"))
        .text(padded)
        .content_type("application/json")
        .await;

    assert_eq!(
        response.status_code(),
        413,
        "the limit must apply to received bytes"
    );
}

#[tokio::test]
async fn pin_lookup_is_case_insensitive() {
    let (server, _store) = server();
    let pin = allocate(&server, "case").await;

    let response = server
        .put(&format!("/pin/case/{}", pin.to_lowercase()))
        .json(&json!({"ok": true}))
        .await;

    assert_eq!(
        response.status_code(),
        202,
        "a lowercase pin must resolve to the same slot"
    );
}

#[tokio::test]
async fn malformed_path_components_are_rejected() {
    let (server, _store) = server();

    let cases: [(&str, u16); 5] = [
        ("ok-namespace_1", 200),
        ("tenant:eu", 400),
        ("has.dot", 400),
        ("日本語", 400),
        ("wayyy-too-long", 400),
    ];

    for (namespace, expected) in cases {
        let namespace = if namespace == "wayyy-too-long" {
            "x".repeat(500)
        } else {
            namespace.to_string()
        };
        let status = server
            .post(&format!("/pin/{namespace}"))
            .await
            .status_code();
        assert_eq!(
            status,
            expected,
            "namespace {:?} returned {status}",
            &namespace[..namespace.len().min(32)]
        );
    }
}

#[tokio::test]
async fn wrong_shaped_pins_are_rejected() {
    let (server, _store) = server();

    for pin in ["AB", "ABCDE", "AB!D"] {
        let status = server
            .post(&format!("/pin/shape/{pin}"))
            .await
            .status_code();
        assert_eq!(status, 400, "pin {pin:?} returned {status}");
    }
}

#[tokio::test]
async fn errors_are_json() {
    let (server, _store) = server();

    let response = server.post("/pin/jsonerr/ZZZZ").await;
    assert_eq!(response.status_code(), 404);
    let body: Value = response.json();
    assert!(
        body.get("error").is_some(),
        "error responses should carry a JSON body, got {body:?}"
    );
}

#[tokio::test]
async fn allocation_past_capacity_is_unavailable_with_retry_after() {
    let (server, _store) = server_with(Config {
        max_entries: 2,
        ..Config::default()
    });

    for _ in 0..2 {
        assert_eq!(server.post("/pin/cap").await.status_code(), 200);
    }

    let response = server.post("/pin/cap").await;
    assert_eq!(response.status_code(), 503);
    assert!(response.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn full_exchange_delivers_once_then_the_pin_is_gone() {
    let (server, _store) = server();
    let pin = allocate(&server, "workflow").await;

    let pending: PinResponse = server.post(&format!("/pin/workflow/{pin}")).await.json();
    assert_eq!(pending.pin, pin, "an empty pin echoes itself back");
    assert!(pending.result.is_none());

    let submitted = server
        .put(&format!("/pin/workflow/{pin}"))
        .json(&json!({"message": "Hello, World!", "number": 42, "array": [1, 2, 3]}))
        .await;
    assert_eq!(submitted.status_code(), 202);

    let delivered: PinResponse = server.post(&format!("/pin/workflow/{pin}")).await.json();
    let result = delivered.result.expect("payload should be delivered");
    assert_eq!(result.get("message").unwrap(), &json!("Hello, World!"));
    assert_eq!(result.get("array").unwrap(), &json!([1, 2, 3]));

    assert_eq!(
        server
            .post(&format!("/pin/workflow/{pin}"))
            .await
            .status_code(),
        404,
        "reading destroys the pin"
    );
}

#[tokio::test]
async fn namespaces_with_the_same_pin_are_isolated() {
    let (server, _store) = server();
    let pin = allocate(&server, "ns1").await;

    server
        .put(&format!("/pin/ns1/{pin}"))
        .json(&json!({"namespace": "ns1"}))
        .await;

    assert_eq!(
        server
            .put(&format!("/pin/ns2/{pin}"))
            .json(&json!({"namespace": "ns2"}))
            .await
            .status_code(),
        404,
        "the same pin string in another namespace is a different slot"
    );

    let delivered: PinResponse = server.post(&format!("/pin/ns1/{pin}")).await.json();
    assert_eq!(
        delivered.result.unwrap().get("namespace").unwrap(),
        &json!("ns1")
    );
}

#[tokio::test]
async fn non_object_and_malformed_bodies_are_rejected() {
    let (server, _store) = server();
    let pin = allocate(&server, "shapes").await;

    for body in ["[1,2,3]", "\"scalar\"", "{not json"] {
        let status = server
            .put(&format!("/pin/shapes/{pin}"))
            .text(body)
            .content_type("application/json")
            .await
            .status_code();
        assert_eq!(status, 422, "body {body:?} returned {status}");
    }
}

#[tokio::test]
async fn health_is_plain_and_cheap() {
    let (server, store) = server();
    let response = server.get("/health").await;

    assert_eq!(response.status_code(), 200);
    assert_eq!(response.text(), "All good.");
    assert_eq!(store.len(), 0, "health must not touch state");
}
