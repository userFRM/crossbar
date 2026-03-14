use crossbar::prelude::*;

// ═══════════════════════════════════════════════════════
// Method
// ═══════════════════════════════════════════════════════

#[test]
fn method_display() {
    assert_eq!(format!("{}", Method::Get), "GET");
    assert_eq!(format!("{}", Method::Post), "POST");
    assert_eq!(format!("{}", Method::Put), "PUT");
    assert_eq!(format!("{}", Method::Delete), "DELETE");
    assert_eq!(format!("{}", Method::Patch), "PATCH");
}

#[test]
fn method_as_str() {
    assert_eq!(Method::Get.as_str(), "GET");
    assert_eq!(Method::Post.as_str(), "POST");
    assert_eq!(Method::Put.as_str(), "PUT");
    assert_eq!(Method::Delete.as_str(), "DELETE");
    assert_eq!(Method::Patch.as_str(), "PATCH");
}

#[test]
fn method_u8_roundtrip() {
    let methods = [
        Method::Get,
        Method::Post,
        Method::Put,
        Method::Delete,
        Method::Patch,
    ];
    for m in methods {
        let byte = u8::from(m);
        let recovered = Method::try_from(byte);
        assert_eq!(recovered, Ok(m), "roundtrip failed for {:?}", m);
    }
}

#[test]
fn method_into_u8_values() {
    assert_eq!(u8::from(Method::Get), 0);
    assert_eq!(u8::from(Method::Post), 1);
    assert_eq!(u8::from(Method::Put), 2);
    assert_eq!(u8::from(Method::Delete), 3);
    assert_eq!(u8::from(Method::Patch), 4);
}

#[test]
fn method_try_from_u8_invalid() {
    assert!(Method::try_from(5u8).is_err());
    assert_eq!(Method::try_from(100u8), Err(100));
    assert_eq!(Method::try_from(255u8), Err(255));
}

#[test]
fn method_clone_copy_eq() {
    let m = Method::Get;
    let m2 = m; // Copy
    let m3 = m; // Copy
    assert_eq!(m, m2);
    assert_eq!(m, m3);
}

// ═══════════════════════════════════════════════════════
// Request method helpers
// ═══════════════════════════════════════════════════════

#[test]
fn request_is_get() {
    assert!(Request::new(Method::Get, "/").is_get());
    assert!(!Request::new(Method::Post, "/").is_get());
}

#[test]
fn request_is_post() {
    assert!(Request::new(Method::Post, "/").is_post());
    assert!(!Request::new(Method::Get, "/").is_post());
}

// ═══════════════════════════════════════════════════════
// Uri
// ═══════════════════════════════════════════════════════

#[test]
fn uri_parse_plain_path() {
    let uri = Uri::parse("/hello/world");
    assert_eq!(uri.path(), "/hello/world");
    assert_eq!(uri.query(), None);
    assert_eq!(uri.raw(), "/hello/world");
}

#[test]
fn uri_parse_with_query() {
    let uri = Uri::parse("/search?q=rust&page=2");
    assert_eq!(uri.path(), "/search");
    assert_eq!(uri.query(), Some("q=rust&page=2"));
}

#[test]
fn uri_parse_with_scheme() {
    let uri = Uri::parse("app://localhost/api/data");
    assert_eq!(uri.path(), "/api/data");
    assert_eq!(uri.query(), None);
}

#[test]
fn uri_parse_with_scheme_and_query() {
    let uri = Uri::parse("app://localhost/api/data?key=val");
    assert_eq!(uri.path(), "/api/data");
    assert_eq!(uri.query(), Some("key=val"));
}

#[test]
fn uri_parse_scheme_no_path() {
    let uri = Uri::parse("app://localhost");
    // No slash after authority => remainder is "/"
    assert_eq!(uri.path(), "/");
    assert_eq!(uri.query(), None);
}

#[test]
fn uri_parse_root() {
    let uri = Uri::parse("/");
    assert_eq!(uri.path(), "/");
    assert_eq!(uri.query(), None);
}

#[test]
fn uri_parse_empty_query() {
    let uri = Uri::parse("/path?");
    assert_eq!(uri.path(), "/path");
    assert_eq!(uri.query(), Some(""));
}

#[test]
fn uri_raw_preserves_original() {
    let raw = "app://localhost/api?q=1";
    let uri = Uri::parse(raw);
    assert_eq!(uri.raw(), raw);
}

#[test]
fn uri_display() {
    let uri = Uri::parse("/api/v1?key=val");
    assert_eq!(format!("{uri}"), "/api/v1?key=val");
}

#[test]
fn uri_from_str() {
    let uri: Uri = "/hello".into();
    assert_eq!(uri.path(), "/hello");
}

// ═══════════════════════════════════════════════════════
// Request
// ═══════════════════════════════════════════════════════

#[test]
fn request_new() {
    let req = Request::new(Method::Get, "/test");
    assert_eq!(req.method, Method::Get);
    assert_eq!(req.uri.path(), "/test");
    assert!(req.body.is_empty());
    assert!(req.headers.is_empty());
}

#[test]
fn request_with_body() {
    let req = Request::new(Method::Post, "/data").with_body("hello");
    assert_eq!(req.body.as_ref(), b"hello");
}

#[test]
fn request_with_body_bytes() {
    let data: &[u8] = &[0x00, 0x01, 0x02, 0x03];
    let req = Request::new(Method::Post, "/bin").with_body(data);
    assert_eq!(req.body.as_ref(), &[0, 1, 2, 3]);
}

#[test]
fn request_path_param_not_set() {
    let req = Request::new(Method::Get, "/items/42");
    // path_params is empty because it's only filled during routing
    assert_eq!(req.path_param("id"), None);
}

#[test]
fn request_query_param() {
    let req = Request::new(Method::Get, "/search?q=hello&page=5");
    assert_eq!(req.query_param("q"), Some("hello".to_string()));
    assert_eq!(req.query_param("page"), Some("5".to_string()));
    assert_eq!(req.query_param("missing"), None);
}

#[test]
fn request_query_params() {
    let req = Request::new(Method::Get, "/search?a=1&b=2&c=3");
    let params = req.query_params();
    assert_eq!(params.len(), 3);
    assert_eq!(params.get("a").map(|s| s.as_str()), Some("1"));
    assert_eq!(params.get("b").map(|s| s.as_str()), Some("2"));
    assert_eq!(params.get("c").map(|s| s.as_str()), Some("3"));
}

#[test]
fn request_query_params_empty() {
    let req = Request::new(Method::Get, "/path");
    let params = req.query_params();
    assert!(params.is_empty());
}

#[test]
fn request_json_body() {
    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Payload {
        name: String,
        value: i32,
    }
    let json = r#"{"name":"test","value":42}"#;
    let req = Request::new(Method::Post, "/data").with_body(json);
    let payload: Payload = req.json_body().unwrap();
    assert_eq!(payload.name, "test");
    assert_eq!(payload.value, 42);
}

#[test]
fn request_json_body_invalid() {
    let req = Request::new(Method::Post, "/data").with_body("not json");
    let result: Result<serde_json::Value, _> = req.json_body();
    assert!(result.is_err());
}

// ═══════════════════════════════════════════════════════
// Response
// ═══════════════════════════════════════════════════════

#[test]
fn response_ok() {
    let resp = Response::ok();
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
}

#[test]
fn response_with_status() {
    let resp = Response::with_status(404);
    assert_eq!(resp.status, 404);
    assert!(resp.body.is_empty());
}

#[test]
fn response_with_body() {
    let resp = Response::ok().with_body("hello");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello");
}

#[test]
fn response_json() {
    #[derive(serde::Serialize)]
    struct Data {
        x: i32,
    }
    let resp = Response::json(&Data { x: 42 });
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.headers.get("content-type").map(|s| s.as_str()),
        Some("application/json")
    );
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(body["x"], 42);
}

#[test]
fn response_not_found() {
    let resp = Response::not_found();
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body_str(), "not found");
}

#[test]
fn response_bad_request() {
    let resp = Response::bad_request("invalid input");
    assert_eq!(resp.status, 400);
    assert_eq!(resp.body_str(), "invalid input");
}

#[test]
fn response_bad_request_string() {
    let msg = String::from("bad data");
    let resp = Response::bad_request(msg);
    assert_eq!(resp.status, 400);
    assert_eq!(resp.body_str(), "bad data");
}

#[test]
fn response_body_str_binary() {
    let resp = Response::ok().with_body(&[0xFFu8, 0xFE][..]);
    // Invalid UTF-8 should return "<binary>"
    assert_eq!(resp.body_str(), "<binary>");
}

#[test]
fn response_body_str_empty() {
    let resp = Response::ok();
    assert_eq!(resp.body_str(), "");
}

#[test]
fn response_with_header() {
    let resp = Response::ok()
        .with_header("x-custom", "value123")
        .with_header("cache-control", "no-store");
    assert_eq!(
        resp.headers.get("x-custom").map(|s| s.as_str()),
        Some("value123")
    );
    assert_eq!(
        resp.headers.get("cache-control").map(|s| s.as_str()),
        Some("no-store")
    );
}

// ═══════════════════════════════════════════════════════
// IntoResponse impls
// ═══════════════════════════════════════════════════════

#[test]
fn into_response_static_str() {
    let resp: Response = "hello".into_response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "hello");
}

#[test]
fn into_response_string() {
    let resp: Response = String::from("world").into_response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "world");
}

#[test]
fn into_response_json() {
    #[derive(serde::Serialize)]
    struct Item {
        id: u32,
    }
    let resp = Json(Item { id: 1 }).into_response();
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["id"], 1);
}

#[test]
fn into_response_tuple_u16_str() {
    let resp: Response = (201u16, "created").into_response();
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body_str(), "created");
}

#[test]
fn into_response_tuple_u16_string() {
    let resp: Response = (500u16, String::from("error")).into_response();
    assert_eq!(resp.status, 500);
    assert_eq!(resp.body_str(), "error");
}

#[test]
fn into_response_result_ok() {
    let result: Result<&str, &str> = Ok("success");
    let resp = result.into_response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "success");
}

#[test]
fn into_response_result_err() {
    let result: Result<&str, (u16, &str)> = Err((500u16, "failed"));
    let resp = result.into_response();
    assert_eq!(resp.status, 500);
    assert_eq!(resp.body_str(), "failed");
}

#[test]
fn into_response_result_ok_json() {
    #[derive(serde::Serialize)]
    struct Ok {
        data: String,
    }
    let result: Result<Json<Ok>, &str> = std::result::Result::Ok(Json(Ok {
        data: "hello".into(),
    }));
    let resp = result.into_response();
    assert_eq!(resp.status, 200);
}

#[test]
fn into_response_body() {
    let b = Body::from(&b"raw bytes"[..]);
    let resp = b.into_response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "raw bytes");
}

#[test]
fn into_response_vec_u8() {
    let v: Vec<u8> = b"vec bytes".to_vec();
    let resp = v.into_response();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body_str(), "vec bytes");
}

#[test]
fn into_response_response_passthrough() {
    let original = Response::with_status(418).with_body("teapot");
    let resp = original.into_response();
    assert_eq!(resp.status, 418);
    assert_eq!(resp.body_str(), "teapot");
}

// ═══════════════════════════════════════════════════════
// percent_decode
// ═══════════════════════════════════════════════════════

#[test]
fn percent_decode_basic() {
    assert_eq!(percent_decode("hello%20world"), "hello world");
}

#[test]
fn percent_decode_multiple() {
    assert_eq!(percent_decode("%48%65%6C%6C%6F"), "Hello");
}

#[test]
fn percent_decode_plus_as_space() {
    assert_eq!(percent_decode("hello+world"), "hello world");
}

#[test]
fn percent_decode_no_encoding() {
    assert_eq!(percent_decode("plain"), "plain");
}

#[test]
fn percent_decode_empty() {
    assert_eq!(percent_decode(""), "");
}

#[test]
fn percent_decode_incomplete_sequence() {
    // "hello%2" -> "hello" is pushed normally, then '%' is seen, '2' is consumed as
    // the high nibble, but bytes.next() returns None for the low nibble.
    // The (Some(h), Some(l)) pattern fails => only '%' is pushed. '2' was consumed.
    // Result: "hello%"
    assert_eq!(percent_decode("hello%2"), "hello%");

    // Trailing '%' with no hex digits after it: "test" then '%' consumes next two
    // bytes.next() calls => both return None => pushes '%'.
    assert_eq!(percent_decode("test%"), "test%");

    // '%' followed by non-hex: "test%G" => '%' seen, 'G' consumed as h but hex_val
    // returns None => falls through and pushes '%'. 'G' is consumed and lost.
    // Actually: (Some(h), Some(l)) = (Some('G'), Some(<next byte or None>))
    // For "test%G": after '%', bytes.next() => Some('G'), bytes.next() => None
    // So (Some('G'), None) => the if-let doesn't match => pushes '%'.
    assert_eq!(percent_decode("test%G"), "test%");
}

#[test]
fn percent_decode_special_chars() {
    assert_eq!(percent_decode("%2F"), "/");
    assert_eq!(percent_decode("%3F"), "?");
    assert_eq!(percent_decode("%26"), "&");
    assert_eq!(percent_decode("%3D"), "=");
}

// ═══════════════════════════════════════════════════════
// parse_query
// ═══════════════════════════════════════════════════════

#[test]
fn parse_query_basic() {
    let params = parse_query("key=val");
    assert_eq!(params.get("key").map(|s| s.as_str()), Some("val"));
}

#[test]
fn parse_query_multiple() {
    let params = parse_query("a=1&b=2&c=3");
    assert_eq!(params.len(), 3);
    assert_eq!(params.get("a").map(|s| s.as_str()), Some("1"));
    assert_eq!(params.get("b").map(|s| s.as_str()), Some("2"));
    assert_eq!(params.get("c").map(|s| s.as_str()), Some("3"));
}

#[test]
fn parse_query_empty() {
    let params = parse_query("");
    assert!(params.is_empty());
}

#[test]
fn parse_query_encoded_values() {
    let params = parse_query("name=hello%20world&path=%2Fhome");
    assert_eq!(params.get("name").map(|s| s.as_str()), Some("hello world"));
    assert_eq!(params.get("path").map(|s| s.as_str()), Some("/home"));
}

#[test]
fn parse_query_empty_value() {
    let params = parse_query("key=");
    assert_eq!(params.get("key").map(|s| s.as_str()), Some(""));
}

#[test]
fn parse_query_no_equals() {
    let params = parse_query("flag");
    assert_eq!(params.get("flag").map(|s| s.as_str()), Some(""));
}

#[test]
fn parse_query_encoded_key() {
    let params = parse_query("hello%20world=value");
    assert_eq!(params.get("hello world").map(|s| s.as_str()), Some("value"));
}

#[test]
fn parse_query_multiple_ampersands() {
    // Consecutive ampersands produce empty segments which are filtered
    let params = parse_query("a=1&&b=2");
    assert_eq!(params.len(), 2);
    assert_eq!(params.get("a").map(|s| s.as_str()), Some("1"));
    assert_eq!(params.get("b").map(|s| s.as_str()), Some("2"));
}

#[test]
fn parse_query_plus_in_value() {
    let params = parse_query("q=hello+world");
    assert_eq!(params.get("q").map(|s| s.as_str()), Some("hello world"));
}
