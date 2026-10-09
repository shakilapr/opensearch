//! HTTP server: `POST /search`, `GET /health` (spec §7).
//!
//! The router is built around a [`Searcher`] trait object held in an `Arc`, so
//! the whole thing is cheap to clone and trivially stubbed in tests. A tower
//! concurrency + load-shed layer bounds in-flight requests so the process can't
//! be flooded; saturated capacity sheds with `429`.

use crate::deep::{DeepSearchRequest, DeepSearchResponse, DeepSearcher, DocResponse};
use crate::engine::{run_engine_search, EngineSearchRequest};
use crate::query::{self, Searcher, DEFAULT_LIMIT};
use axum::{
    extract::{rejection::JsonRejection, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    BoxError, Json, Router,
};
use search_core::config::Config;
use search_core::{AppError, AppResult};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Instant;

/// Max in-flight requests before the load-shed layer starts returning `429`.
pub const DEFAULT_MAX_CONCURRENCY: usize = 64;

/// Cloneable, cheap (single `Arc`) application state.
#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<Inner>,
}

pub struct Inner {
    pub searcher: Arc<dyn Searcher + Send + Sync>,
    /// Optional deep-search / document-read capability (`/search_deep`, `/read`).
    deep: Option<Arc<dyn DeepSearcher + Send + Sync>>,
    /// Optional §22.1 answer generator; `None` keeps the engine lexical-only.
    answerer: Option<Arc<dyn search_engine::Answerer + Send + Sync>>,
    /// Extra retrieval lanes fused into every engine query (today: the dense lane). Built by
    /// the binary, which owns the feature and environment decisions.
    extra_lanes: Vec<Arc<dyn search_retrieval::LaneSearcher + Send + Sync>>,
    started: Instant,
}

impl AppState {
    pub fn new(searcher: Arc<dyn Searcher + Send + Sync>) -> Self {
        Self {
            inner: Arc::new(Inner {
                searcher,
                deep: None,
                answerer: None,
                extra_lanes: Vec::new(),
                started: Instant::now(),
            }),
        }
    }

    /// Build state that also serves `/search_deep` and `/read`.
    pub fn with_deep(
        searcher: Arc<dyn Searcher + Send + Sync>,
        deep: Arc<dyn DeepSearcher + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                searcher,
                deep: Some(deep),
                answerer: None,
                extra_lanes: Vec::new(),
                started: Instant::now(),
            }),
        }
    }

    /// Attach the optional §22.1 answer generator used by the engine path.
    /// Preserves the existing searcher/deep wiring.
    pub fn with_answerer(
        self,
        answerer: Arc<dyn search_engine::Answerer + Send + Sync>,
    ) -> Self {
        let searcher = self.inner.searcher.clone();
        let deep = self.inner.deep.clone();
        let extra_lanes = self.inner.extra_lanes.clone();
        let started = self.inner.started;
        Self {
            inner: Arc::new(Inner {
                searcher,
                deep,
                answerer: Some(answerer),
                extra_lanes,
                started,
            }),
        }
    }

    /// Attach extra retrieval lanes (e.g. the dense lane over a vector store). They are fused
    /// into every engine query; each lane names itself in `lane_ranks`.
    pub fn with_extra_lanes(
        self,
        extra_lanes: Vec<Arc<dyn search_retrieval::LaneSearcher + Send + Sync>>,
    ) -> Self {
        let searcher = self.inner.searcher.clone();
        let deep = self.inner.deep.clone();
        let answerer = self.inner.answerer.clone();
        let started = self.inner.started;
        Self {
            inner: Arc::new(Inner { searcher, deep, answerer, extra_lanes, started }),
        }
    }

    pub fn searcher(&self) -> &(dyn Searcher + Send + Sync) {
        self.inner.searcher.as_ref()
    }

    /// The deep-search backend, when one was configured.
    pub fn deep(&self) -> Option<&(dyn DeepSearcher + Send + Sync)> {
        self.inner.deep.as_deref()
    }

    /// The answer generator, when one was configured.
    pub fn answerer(&self) -> Option<&(dyn search_engine::Answerer + Send + Sync)> {
        self.inner.answerer.as_deref()
    }

    pub fn uptime_secs(&self) -> u64 {
        self.inner.started.elapsed().as_secs()
    }
}

/// Error type returned by handlers; maps every failure onto an HTTP status.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn from_app(e: AppError) -> Self {
        let status = match &e {
            AppError::Invalid(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Deadline => StatusCode::GATEWAY_TIMEOUT,
            // "busy" — a resource bound means try again later.
            AppError::LimitExceeded(_) => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: e.to_string(),
        }
    }

    fn from_rejection(r: JsonRejection) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: format!("invalid request body: {r}"),
        }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }

    /// The deep-search backend was not configured for this server.
    fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "deep search is not configured on this server".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": self.message,
            "status": self.status.as_u16(),
        }));
        (self.status, body).into_response()
    }
}

/// Build the application router.
/// Build the CORS layer from `SEARCH_CORS_ORIGIN` (comma-separated origins).
///
/// **Off by default, and deliberately narrow.** The API binds loopback and holds no
/// authentication, so accepting any origin would let any web page the user visits drive their
/// local index (a classic localhost/DNS-rebinding surface). Only the origins named here are
/// allowed, and only the two methods and one header the UI actually sends. The production shape
/// is same-origin — serve the built UI from the same host (`deploy/nginx.conf`) — which needs no
/// CORS at all.
fn cors_layer_from_env() -> Option<tower_http::cors::CorsLayer> {
    let raw = std::env::var("SEARCH_CORS_ORIGIN").ok()?;
    let names: Vec<String> = raw.split(',').map(|o| o.trim().to_string()).collect();
    match cors_layer_from_origins(&names) {
        Some(layer) => {
            tracing::info!(origins = %raw, "CORS enabled for the named origins");
            Some(layer)
        }
        None => {
            tracing::warn!("SEARCH_CORS_ORIGIN set but no origin parsed; CORS stays off");
            None
        }
    }
}

/// Build the CORS layer for an explicit origin list, or `None` when the list is empty.
///
/// Public so tests exercise *this* construction rather than a hand-rolled approximation — which
/// matters, because the two are not equivalent: `CorsLayer::allow_origin` with a **single**
/// `HeaderValue` emits that value for every request, while a **list** matches the request's
/// `Origin` and sends nothing when it is not on it. Only the list form enforces the allow-list.
pub fn cors_layer_from_origins(origins: &[String]) -> Option<tower_http::cors::CorsLayer> {
    use tower_http::cors::CorsLayer;
    let allowed: Vec<axum::http::HeaderValue> = origins
        .iter()
        .filter_map(|o| o.parse::<axum::http::HeaderValue>().ok())
        .collect();
    if allowed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(allowed)
            .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
            .allow_headers([axum::http::header::CONTENT_TYPE]),
    )
}

/// The routes with no CORS layer (same-origin deployments, and tests).
pub fn router(state: AppState) -> Router {
    router_with_cors(state, None)
}

/// The routes, with an optional CORS layer (see [`cors_layer_from_env`]).
pub fn router_with_cors(state: AppState, cors: Option<tower_http::cors::CorsLayer>) -> Router {
    // Outer->inner: map shed errors to 429, shed when full, then bound the
    // number of concurrently-running handlers.
    let bounded = tower::ServiceBuilder::new()
        .layer(axum::error_handling::HandleErrorLayer::new(|_: BoxError| async {
            StatusCode::TOO_MANY_REQUESTS
        }))
        .layer(tower::load_shed::LoadShedLayer::new())
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            DEFAULT_MAX_CONCURRENCY,
        ));

    let app = Router::new()
        .route("/health", get(health))
        .route("/search", post(search))
        .route("/search_deep", post(search_deep))
        .route("/read", get(read_doc))
        .layer(bounded)
        .with_state(state);
    match cors {
        Some(layer) => app.layer(layer),
        None => app,
    }
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let (generation, manifest_id) = match state.searcher().manifest() {
        Ok(m) => (m.generation, m.manifest_id),
        Err(_) => (0, "unavailable".to_string()),
    };
    Json(serde_json::json!({
        "status": "ok",
        "generation": generation,
        "index_manifest_id": manifest_id,
        "uptime_seconds": state.uptime_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn search(
    State(state): State<AppState>,
    req: Result<Json<EngineSearchRequest>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(req) = req.map_err(ApiError::from_rejection)?;

    // Search is CPU/IO bound; keep it off the async runtime's worker threads.
    let searcher = state.inner.searcher.clone();
    let query_len = req.base.query.len();
    let limit = req.base.limit;

    if req.engine {
        // §6–§18 engine path. The clock is read HERE, at the HTTP boundary, and
        // passed into the engine as unix seconds.
        let answerer = state.inner.answerer.clone();
        let extra_lanes = state.inner.extra_lanes.clone();
        let now_ms = now_unix_s();
        let resp = tokio::task::spawn_blocking(move || {
            let answerer_ref: Option<&dyn search_engine::Answerer> =
                answerer.as_deref().map(|a| a as &dyn search_engine::Answerer);
            run_engine_search(searcher.as_ref(), req, now_ms, answerer_ref, &extra_lanes)
        })
        .await
        .map_err(|e| ApiError::internal(format!("search task panicked: {e}")))?
        .map_err(ApiError::from_app)?;

        tracing::debug!(
            query_len,
            limit,
            hits = resp.results.len(),
            stop_reason = %resp.stop_reason,
            "POST /search (engine) served"
        );
        let value =
            serde_json::to_value(&resp).map_err(|e| ApiError::internal(e.to_string()))?;
        return Ok(Json(value));
    }

    // Legacy path: bit-for-bit unchanged when `engine` is absent/false.
    let resp = tokio::task::spawn_blocking(move || query::execute(searcher.as_ref(), &req.base))
        .await
        .map_err(|e| ApiError::internal(format!("search task panicked: {e}")))?
        .map_err(ApiError::from_app)?;

    tracing::debug!(
        query_len,
        limit,
        hits = resp.results.len(),
        "POST /search served"
    );
    let value = serde_json::to_value(&resp).map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(value))
}

/// Current wall-clock time in unix seconds. Read only at the HTTP boundary; the
/// engine core never touches a clock.
fn now_unix_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `POST /search_deep` — navigation-driven search returning hits + trace.
async fn search_deep(
    State(state): State<AppState>,
    req: Result<Json<DeepSearchRequest>, JsonRejection>,
) -> Result<Json<DeepSearchResponse>, ApiError> {
    let Json(req) = req.map_err(ApiError::from_rejection)?;

    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "query must not be empty".to_string(),
        });
    }
    let limit = query::normalize_limit(req.limit.unwrap_or(DEFAULT_LIMIT))
        .map_err(ApiError::from_app)?;

    let deep = state
        .inner
        .deep
        .clone()
        .ok_or_else(ApiError::unavailable)?;
    // Navigation runs synchronous index searches: keep it off the async workers.
    let resp = tokio::task::spawn_blocking(move || deep.deep_search(&query, limit))
        .await
        .map_err(|e| ApiError::internal(format!("deep search task panicked: {e}")))?
        .map_err(ApiError::from_app)?;

    Ok(Json(resp))
}

/// Query parameters for `GET /read`.
#[derive(Debug, Deserialize)]
struct ReadParams {
    doc_id: Option<String>,
}

/// `GET /read?doc_id=` — the normalized text of a previously-seen document.
async fn read_doc(
    State(state): State<AppState>,
    Query(params): Query<ReadParams>,
) -> Result<Json<DocResponse>, ApiError> {
    let doc_id = params
        .doc_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "missing required query parameter 'doc_id'".to_string(),
        })?;

    let deep = state
        .inner
        .deep
        .clone()
        .ok_or_else(ApiError::unavailable)?;
    let resp = tokio::task::spawn_blocking(move || deep.read(&doc_id))
        .await
        .map_err(|e| ApiError::internal(format!("read task panicked: {e}")))?
        .map_err(ApiError::from_app)?;

    Ok(Json(resp))
}

/// Bind and serve until the process is stopped.
///
/// `deep` optionally enables `/search_deep` and `/read`; when `None` those
/// routes answer `503`.
pub async fn serve(
    cfg: &Config,
    searcher: Arc<dyn Searcher + Send + Sync>,
    deep: Option<Arc<dyn DeepSearcher + Send + Sync>>,
) -> AppResult<()> {
    serve_with(cfg, searcher, deep, None, Vec::new()).await
}

/// Bind and serve until the process is stopped, with an optional §22.1 answer
/// generator for the engine path.
///
/// `deep` optionally enables `/search_deep` and `/read` (else `503`);
/// `answerer` optionally enables the `answer` field on `POST /search` when
/// `engine: true` (else the engine is lexical-only).
/// Serve, optionally with extra retrieval lanes (see [`AppState::with_extra_lanes`]).
pub async fn serve_with(
    cfg: &Config,
    searcher: Arc<dyn Searcher + Send + Sync>,
    deep: Option<Arc<dyn DeepSearcher + Send + Sync>>,
    answerer: Option<Arc<dyn search_engine::Answerer + Send + Sync>>,
    extra_lanes: Vec<Arc<dyn search_retrieval::LaneSearcher + Send + Sync>>,
) -> AppResult<()> {
    // Ensure a subscriber exists even when the caller (e.g. an embedding binary)
    // forgot to initialise one. Idempotent.
    crate::init_tracing();
    let state = match deep {
        Some(d) => AppState::with_deep(searcher, d),
        None => AppState::new(searcher),
    };
    let state = match answerer {
        Some(a) => state.with_answerer(a),
        None => state,
    };
    let state = if extra_lanes.is_empty() {
        state
    } else {
        tracing::info!(lanes = extra_lanes.len(), "serving with injected retrieval lanes");
        state.with_extra_lanes(extra_lanes)
    };
    let app = router_with_cors(state, cors_layer_from_env());
    let listener = tokio::net::TcpListener::bind(&cfg.bind)
        .await
        .map_err(|e| AppError::Config(format!("failed to bind {}: {e}", cfg.bind)))?;
    tracing::info!(addr = %cfg.bind, "search-api listening");
    axum::serve(listener, app).await?;
    Ok(())
}
