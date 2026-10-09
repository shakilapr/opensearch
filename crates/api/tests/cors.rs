//! CORS is opt-in and narrow: the API binds loopback with no authentication, so an
//! allow-any-origin default would let any page the user visits drive their local index.
//!
//! These drive the router directly (`oneshot`, no socket), which is also how the preflight
//! behaviour is defined: a preflight for an origin that is not on the list is answered *without*
//! `access-control-allow-origin`, which is what makes the browser refuse the real request.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use tower::ServiceExt;

use search_api::{AppState, IndexHit, Searcher};
use search_core::{AppError, AppResult, IndexManifest, SearchFilters};

/// A searcher that is never reached by these tests: they only exercise the layer.
struct StubSearcher;

impl Searcher for StubSearcher {
    fn search(
        &self,
        _q: &str,
        _limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        Ok(Vec::new())
    }

    fn manifest(&self) -> AppResult<IndexManifest> {
        Err(AppError::Index("stub".into()))
    }
}

fn preflight(origin: &str) -> Request<Body> {
    Request::builder()
        .method(Method::OPTIONS)
        .uri("/search")
        .header(header::ORIGIN, origin)
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn a_listed_origin_is_allowed_and_an_unlisted_one_is_not() {
    use std::sync::Arc;

    let searcher: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    // The production constructor, so this test cannot pass against a different (weaker) layer.
    let layer = search_api::cors_layer_from_origins(&["http://127.0.0.1:5173".to_string()])
        .expect("a non-empty origin list yields a layer");
    let app = search_api::router_with_cors(AppState::new(searcher), Some(layer));

    let resp = app.clone().oneshot(preflight("http://127.0.0.1:5173")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap()),
        Some("http://127.0.0.1:5173"),
        "a listed origin must be allowed"
    );

    let resp = app.oneshot(preflight("http://evil.example")).await.unwrap();
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "an unlisted origin must not receive an allow-origin header, or the browser would let \
         any page drive the local index"
    );
}

#[tokio::test]
async fn the_default_router_sends_no_cors_headers_at_all() {
    use std::sync::Arc;

    let searcher: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    let app = search_api::router(AppState::new(searcher));
    let resp = app.oneshot(preflight("http://127.0.0.1:5173")).await.unwrap();
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "without SEARCH_CORS_ORIGIN the API must stay same-origin only"
    );
}
