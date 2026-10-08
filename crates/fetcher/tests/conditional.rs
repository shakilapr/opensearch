//! Conditional refresh (crawler §18) — hermetic tests against a local axum
//! server. Uses the fetcher's test-only SSRF escape hatch so 127.0.0.1 is
//! reachable; never the public internet.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;

use search_core::Limits;
use search_fetcher::canon::canonicalize;
use search_fetcher::{Fetched, Fetcher, MemoryValidators, ValidatorStore};

/// One scripted server response.
#[derive(Clone)]
enum Spec {
    Ok {
        body: &'static str,
        etag: Option<&'static str>,
        last_modified: Option<&'static str>,
    },
    NotModified,
    NotFound,
}

struct Script {
    responses: Vec<Spec>,
    /// Request headers observed, in order.
    seen: Vec<HeaderMap>,
    idx: usize,
}

#[derive(Clone)]
struct AppState(Arc<Mutex<Script>>);

fn build_response(spec: &Spec) -> Response {
    match spec {
        Spec::Ok {
            body,
            etag,
            last_modified,
        } => {
            let mut b = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html");
            if let Some(e) = etag {
                b = b.header(header::ETAG, *e);
            }
            if let Some(l) = last_modified {
                b = b.header(header::LAST_MODIFIED, *l);
            }
            b.body(Body::from(*body)).unwrap()
        }
        Spec::NotModified => Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap(),
        Spec::NotFound => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}

async fn handler(State(st): State<AppState>, headers: HeaderMap) -> Response {
    // std::sync::Mutex is fine: no await while the guard is held.
    let spec = {
        let mut g = st.0.lock().unwrap();
        let idx = g.idx;
        g.idx += 1;
        g.seen.push(headers);
        g.responses.get(idx).cloned().unwrap_or(Spec::NotFound)
    };
    build_response(&spec)
}

/// Start the server with a fixed script; returns its address and shared state.
async fn spawn(script: Vec<Spec>) -> (SocketAddr, AppState) {
    let state = AppState(Arc::new(Mutex::new(Script {
        responses: script,
        seen: Vec::new(),
        idx: 0,
    })));
    let app = Router::new()
        .route("/page", get(handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

fn test_fetcher() -> Fetcher {
    Fetcher::new(Limits::default(), "TestBot/0.1".into()).allow_private_addresses_for_tests(true)
}

fn saw_headers(state: &AppState, i: usize) -> HeaderMap {
    state.0.lock().unwrap().seen[i].clone()
}

const LM: &str = "Wed, 21 Oct 2015 07:28:00 GMT";

// Test 1: 200 stores validators; a later conditional request sends the headers
// and a 304 yields Unchanged with no body.
#[tokio::test]
async fn conditional_200_then_304_is_unchanged() {
    let (addr, state) = spawn(vec![
        Spec::Ok {
            body: "hello",
            etag: Some("\"v1\""),
            last_modified: Some(LM),
        },
        Spec::NotModified,
    ])
    .await;
    let url = format!("http://{addr}/page");
    let key = canonicalize(&url).unwrap();
    let store = MemoryValidators::new();
    let f = test_fetcher();

    // First fetch: unconditional 200, body read, validators stored.
    match f.fetch_conditional(&store, &url).await.unwrap() {
        Fetched::Modified {
            status,
            body,
            validators,
            ..
        } => {
            assert_eq!(status, 200);
            assert_eq!(body, b"hello");
            assert_eq!(validators.etag.as_deref(), Some("\"v1\""));
            assert_eq!(validators.last_modified.as_deref(), Some(LM));
            assert!(validators.fetched_at > 0);
        }
        other => panic!("expected Modified, got {other:?}"),
    }
    assert_eq!(store.get(&key).unwrap().etag.as_deref(), Some("\"v1\""));

    // First request must NOT carry conditional headers (nothing stored yet).
    let h0 = saw_headers(&state, 0);
    assert!(h0.get(header::IF_NONE_MATCH).is_none());
    assert!(h0.get(header::IF_MODIFIED_SINCE).is_none());

    // Second fetch: conditional headers go out, server replies 304.
    match f.fetch_conditional(&store, &url).await.unwrap() {
        Fetched::Unchanged {
            validators,
            final_url,
        } => {
            assert!(final_url.ends_with("/page"));
            // 304 carried no new validators → prior values are retained.
            assert_eq!(validators.etag.as_deref(), Some("\"v1\""));
            assert_eq!(validators.last_modified.as_deref(), Some(LM));
        }
        other => panic!("expected Unchanged, got {other:?}"),
    }

    // The server actually received the conditional headers.
    let h1 = saw_headers(&state, 1);
    assert_eq!(
        h1.get(header::IF_NONE_MATCH).unwrap().to_str().unwrap(),
        "\"v1\""
    );
    assert_eq!(
        h1.get(header::IF_MODIFIED_SINCE).unwrap().to_str().unwrap(),
        LM
    );

    // Exactly two requests were made.
    assert_eq!(state.0.lock().unwrap().seen.len(), 2);
}

// Test 2: conditional request answered with a fresh 200 updates body + validators.
#[tokio::test]
async fn conditional_200_with_new_etag_is_modified_and_updates_store() {
    let (addr, state) = spawn(vec![
        Spec::Ok {
            body: "v1-body",
            etag: Some("\"v1\""),
            last_modified: None,
        },
        Spec::Ok {
            body: "v2-body",
            etag: Some("\"v2\""),
            last_modified: Some(LM),
        },
    ])
    .await;
    let url = format!("http://{addr}/page");
    let key = canonicalize(&url).unwrap();
    let store = MemoryValidators::new();
    let f = test_fetcher();

    let _ = f.fetch_conditional(&store, &url).await.unwrap();

    match f.fetch_conditional(&store, &url).await.unwrap() {
        Fetched::Modified {
            status,
            body,
            validators,
            ..
        } => {
            assert_eq!(status, 200);
            assert_eq!(body, b"v2-body");
            assert_eq!(validators.etag.as_deref(), Some("\"v2\""));
            assert_eq!(validators.last_modified.as_deref(), Some(LM));
        }
        other => panic!("expected Modified, got {other:?}"),
    }

    // Second request carried the old validator; store now holds the new one.
    let h1 = saw_headers(&state, 1);
    assert_eq!(
        h1.get(header::IF_NONE_MATCH).unwrap().to_str().unwrap(),
        "\"v1\""
    );
    assert!(h1.get(header::IF_MODIFIED_SINCE).is_none());
    assert_eq!(store.get(&key).unwrap().etag.as_deref(), Some("\"v2\""));
}

// Test 3: no stored validators → a plain unconditional 200, no conditional headers.
#[tokio::test]
async fn missing_validators_sends_plain_200() {
    let (addr, state) = spawn(vec![Spec::Ok {
        body: "fresh",
        etag: None,
        last_modified: None,
    }])
    .await;
    let url = format!("http://{addr}/page");
    let store = MemoryValidators::new();
    let f = test_fetcher();

    match f.fetch_conditional(&store, &url).await.unwrap() {
        Fetched::Modified { body, validators, .. } => {
            assert_eq!(body, b"fresh");
            assert!(!validators.has_any());
        }
        other => panic!("expected Modified, got {other:?}"),
    }

    let h0 = saw_headers(&state, 0);
    assert!(h0.get(header::IF_NONE_MATCH).is_none());
    assert!(h0.get(header::IF_MODIFIED_SINCE).is_none());
}

// Test 4: a non-2xx/non-304 terminal status is surfaced as Other with no body.
#[tokio::test]
async fn not_found_is_other_without_body() {
    let (addr, _state) = spawn(vec![Spec::NotFound]).await;
    let url = format!("http://{addr}/page");
    let store = MemoryValidators::new();
    let f = test_fetcher();

    match f.fetch_conditional(&store, &url).await.unwrap() {
        Fetched::Other { status, .. } => assert_eq!(status, 404),
        other => panic!("expected Other, got {other:?}"),
    }
    assert!(store.is_empty());
}
