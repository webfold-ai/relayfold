//! HTTP surface over [`ControlPlane`]: register, heartbeat, list, and config push to a fleet of
//! relayfold-based proxy instances, served by `servelet-h1`.
//!
//! Status mapping: no/invalid/stale/replayed credentials → **401**; a full replay window →
//! **429**; authenticated but not enrolled (`/register`, `GET /instances`; the body is exactly
//! [`crate::plane::NOT_ENROLLED`]), not the id's bound key, or not an admin → **403**; unknown id →
//! 404; a storage failure → 500 with nothing applied.

use crate::auth::{RawAuth, SignedCall, H_KEY, H_NONCE, H_SIGNATURE, H_TIMESTAMP};
use crate::clock::Clock;
use crate::dto::{
    json_array, ConfigPushRequest, ConfigPushResult, HeartbeatRequest, InstanceView,
    RegisterRequest,
};
use crate::json::Json;
use crate::plane::{ControlConfig, ControlError, ControlPlane, PreparedPush};
use crate::stages::Stages;
use servelet_client::Client;
use servelet_core::{Request, Response, StatusCode};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A request, owned so it can move onto the blocking pool.
pub struct OwnedCall {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
    pub auth: RawAuth,
}

impl OwnedCall {
    pub fn from_request(req: &Request) -> Self {
        let header = |name: &str| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        OwnedCall {
            method: req.method().as_str().to_string(),
            path: req.path().to_string(),
            body: req.body().to_vec(),
            auth: RawAuth {
                key: header(H_KEY),
                timestamp: header(H_TIMESTAMP),
                nonce: header(H_NONCE),
                signature: header(H_SIGNATURE),
            },
        }
    }

    pub fn as_call(&self) -> SignedCall<'_> {
        SignedCall {
            method: &self.method,
            path: &self.path,
            body: &self.body,
            auth: &self.auth,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    plane: Arc<Mutex<ControlPlane>>,
    http: Client,
}

impl AppState {
    /// Open the durable log in `data_dir` and serve with the real decision points.
    pub fn open(
        data_dir: impl AsRef<Path>,
        cfg: ControlConfig,
        clock: Arc<dyn Clock>,
    ) -> std::io::Result<Self> {
        Self::open_with(data_dir, cfg, clock, Stages::REAL)
    }

    pub fn open_with(
        data_dir: impl AsRef<Path>,
        cfg: ControlConfig,
        clock: Arc<dyn Clock>,
        stages: Stages,
    ) -> std::io::Result<Self> {
        let plane = ControlPlane::open(data_dir.as_ref(), cfg, clock, stages)?;
        Ok(Self::from_plane(plane))
    }

    pub fn from_plane(plane: ControlPlane) -> Self {
        AppState {
            plane: Arc::new(Mutex::new(plane)),
            http: Client::new(),
        }
    }

    /// Run `f` on the blocking pool: it may `fsync`, and must not stall an async worker.
    async fn with_plane<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut ControlPlane) -> R + Send + 'static,
    ) -> Result<R, ControlError> {
        let plane = self.plane.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = plane
                .lock()
                .map_err(|_| std::io::Error::other("control plane lock poisoned"))?;
            Ok::<R, std::io::Error>(f(&mut guard))
        })
        .await
        .map_err(|e| ControlError::Storage(std::io::Error::other(e.to_string())))?
        .map_err(ControlError::Storage)
    }

    pub async fn register(
        &self,
        call: OwnedCall,
        req: RegisterRequest,
    ) -> Result<InstanceView, ControlError> {
        self.with_plane(move |p| p.register(&call.as_call(), req))
            .await?
    }

    pub async fn heartbeat(
        &self,
        call: OwnedCall,
        req: HeartbeatRequest,
    ) -> Result<InstanceView, ControlError> {
        self.with_plane(move |p| p.heartbeat(&call.as_call(), req))
            .await?
    }

    /// The unauthenticated in-process view (operator / tests). HTTP uses [`AppState::list_signed`].
    pub async fn list(&self) -> Vec<InstanceView> {
        self.with_plane(|p| p.list()).await.unwrap_or_default()
    }

    /// `GET /instances`: a signed request from an enrolled key.
    pub async fn list_signed(&self, call: OwnedCall) -> Result<Vec<InstanceView>, ControlError> {
        self.with_plane(move |p| p.list_signed(&call.as_call()))
            .await?
    }

    /// Compact the durable log now (see [`ControlPlane::compact`]).
    pub async fn compact(&self) -> Result<crate::compaction::CompactionReport, ControlError> {
        self.with_plane(|p| p.compact()).await?
    }

    pub async fn config_version(&self) -> u64 {
        self.with_plane(|p| p.config_version()).await.unwrap_or(0)
    }

    pub async fn control_public_key(&self) -> crate::auth::Key {
        self.with_plane(|p| p.control_public_key())
            .await
            .unwrap_or([0; 32])
    }

    /// Push `req.config` to every instance matching `req.target` (`"*"` = all). Each push is a
    /// real HTTP POST to `{instance.addr}/relayfold-control/config` carrying a [`SignedConfig`]
    /// envelope; failures (unreachable instance, non-2xx) are reported per instance and never
    /// abort the rest of the fleet.
    ///
    /// [`SignedConfig`]: crate::config::SignedConfig
    pub async fn push_config(
        &self,
        call: OwnedCall,
        req: ConfigPushRequest,
    ) -> Result<Vec<ConfigPushResult>, ControlError> {
        let target = req.target.clone();
        let PreparedPush {
            config_version,
            deliveries,
        } = self
            .with_plane(move |p| p.push_config(&call.as_call(), &req))
            .await??;
        let version = i64::try_from(config_version).unwrap_or(i64::MAX);

        if deliveries.is_empty() {
            return Ok(vec![ConfigPushResult {
                id: target,
                ok: false,
                detail: "no matching instance registered".into(),
                config_version: version,
            }]);
        }

        let mut results = Vec::with_capacity(deliveries.len());
        for d in deliveries {
            let url = format!("{}/relayfold-control/config", d.addr);
            let sent = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .body(d.body)
                .send()
                .await;
            let (ok, detail) = match sent {
                Ok(resp) if resp.status().is_success() => (true, "delivered".to_string()),
                Ok(resp) => (false, format!("instance returned {}", resp.status())),
                Err(e) => (false, format!("delivery failed: {e}")),
            };
            results.push(ConfigPushResult {
                id: d.id,
                ok,
                detail,
                config_version: version,
            });
        }
        Ok(results)
    }
}

fn error_response(e: ControlError) -> Response {
    let (status, msg) = match &e {
        ControlError::Unauthenticated(crate::auth::AuthError::WindowFull) => {
            (StatusCode::TOO_MANY_REQUESTS, e.to_string())
        }
        ControlError::Unauthenticated(_) => (StatusCode::UNAUTHORIZED, e.to_string()),
        ControlError::Forbidden(_) | ControlError::NotEnrolled => {
            (StatusCode::FORBIDDEN, e.to_string())
        }
        ControlError::BadRequest(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        ControlError::NotFound => (StatusCode::NOT_FOUND, e.to_string()),
        // The io error can carry a filesystem path; the operator reads it from the process, not
        // the caller from the wire.
        ControlError::Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "storage error".into()),
    };
    let resp = Response::new(status, msg.into_bytes())
        .with_header("content-type", "text/plain; charset=utf-8");
    if status == StatusCode::UNAUTHORIZED {
        resp.with_header("www-authenticate", "relayfold-dialkey")
    } else {
        resp
    }
}

fn bad_body(what: &str) -> Response {
    Response::new(StatusCode::BAD_REQUEST, format!("invalid {what} body"))
}

fn state_of(req: &Request) -> Option<Arc<AppState>> {
    req.extensions().get::<Arc<AppState>>().cloned()
}

fn state_missing() -> Response {
    Response::new(StatusCode::INTERNAL_SERVER_ERROR, "state missing")
}

async fn handle_register(req: Request) -> Response {
    let Some(state) = state_of(&req) else {
        return state_missing();
    };
    let Ok(Json(body)) = Json::<RegisterRequest>::from_request(&req) else {
        return bad_body("register");
    };
    match state.register(OwnedCall::from_request(&req), body).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => error_response(e),
    }
}

async fn handle_heartbeat(req: Request) -> Response {
    let Some(state) = state_of(&req) else {
        return state_missing();
    };
    let Ok(Json(body)) = Json::<HeartbeatRequest>::from_request(&req) else {
        return bad_body("heartbeat");
    };
    match state.heartbeat(OwnedCall::from_request(&req), body).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => error_response(e),
    }
}

async fn handle_list(req: Request) -> Response {
    let Some(state) = state_of(&req) else {
        return state_missing();
    };
    match state.list_signed(OwnedCall::from_request(&req)).await {
        Ok(views) => json_array(&views),
        Err(e) => error_response(e),
    }
}

async fn handle_config_push(req: Request) -> Response {
    let Some(state) = state_of(&req) else {
        return state_missing();
    };
    let Ok(Json(body)) = Json::<ConfigPushRequest>::from_request(&req) else {
        return bad_body("config push");
    };
    match state.push_config(OwnedCall::from_request(&req), body).await {
        Ok(results) => json_array(&results),
        Err(e) => error_response(e),
    }
}

/// `state` is the raw `AppState`, not pre-wrapped — `Router::with_state` does its own
/// `Arc::new(state)` internally (see its doc: read back via `Arc<S>`); wrapping it again here
/// would inject `Arc<Arc<AppState>>` and every handler's `.get::<Arc<AppState>>()` would miss.
pub fn router(state: AppState) -> servelet_router::Router {
    servelet_router::Router::new()
        .post("/register", handle_register)
        .post("/heartbeat", handle_heartbeat)
        .get("/instances", handle_list)
        .post("/config/push", handle_config_push)
        .with_state(state)
}

/// Accept connections on an already-bound `listener` and serve `router` over `servelet-h1`.
/// `servelet_h1::serve` binds internally and cannot report an ephemeral port; binding here lets
/// a caller bind `127.0.0.1:0`, read `local_addr()`, then serve — and `main` uses this same
/// function, so the tests exercise the production serving path.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    router: servelet_router::Router,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        tokio::spawn(servelet_h1::serve_connection(
            stream,
            router.clone(),
            Some(peer),
        ));
    }
}
