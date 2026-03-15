use crossbar_inproc::prelude::*;

// -- Helpers ----

fn test_router() -> Router {
    Router::new()
        .route("/health", get(|| "ok"))
        .route(
            "/echo",
            post(|req: Request| Response::ok().with_body(req.body.clone())),
        )
        .route(
            "/status/:code",
            get(|req: Request| {
                let code: u16 = req
                    .path_param("code")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200);
                Response::with_status(code).with_body(format!("status:{code}"))
            }),
        )
        .route(
            "/json",
            get(|| {
                let bytes =
                    serde_json::to_vec(&serde_json::json!({"msg": "hello", "num": 42})).unwrap();
                Response::ok()
                    .with_body(bytes)
                    .with_header("content-type", "application/json")
            }),
        )
}

// ===============================================
// InProcessClient
// ===============================================

#[test]
fn inproc_get() {
    let client = InProcessClient::new(test_router());
    let resp = client.get("/health");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "ok");
}

#[test]
fn inproc_post_with_body() {
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", "hello inproc");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello inproc");
}

#[test]
fn inproc_json_response() {
    let client = InProcessClient::new(test_router());
    let resp = client.get("/json");
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["msg"], "hello");
    assert_eq!(v["num"], 42);
}

#[test]
fn inproc_404() {
    let client = InProcessClient::new(test_router());
    let resp = client.get("/nonexistent");
    assert_eq!(resp.status, 404);
}

#[test]
fn inproc_empty_body() {
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", "");
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
}

#[test]
fn inproc_binary_body_roundtrip() {
    let binary: Vec<u8> = (0..=255).collect();
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", binary.clone());
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.as_ref(), binary.as_slice());
}

#[test]
fn inproc_large_payload() {
    let data = vec![b'X'; 1_000_000]; // 1 MB
    let client = InProcessClient::new(test_router());
    let resp = client.post("/echo", data.clone());
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.len(), 1_000_000);
    assert_eq!(resp.body.as_ref(), data.as_slice());
}

#[test]
fn inproc_json_roundtrip() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Item {
        id: u64,
        name: String,
    }

    let router = Router::new().route(
        "/roundtrip",
        post(|req: Request| {
            let item: Item = serde_json::from_slice(&req.body).unwrap();
            let bytes = serde_json::to_vec(&item).unwrap();
            Response::ok()
                .with_body(bytes)
                .with_header("content-type", "application/json")
        }),
    );
    let client = InProcessClient::new(router);

    let input = Item {
        id: 123,
        name: "test".into(),
    };
    let body = serde_json::to_vec(&input).unwrap();
    let resp = client.post("/roundtrip", body);
    assert_eq!(resp.status, 200);
    let output: Item = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(input, output);
}
