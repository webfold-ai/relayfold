//! The control plane's real surface: register, heartbeat, list, and push config to a fleet of
//! relayfold-based proxy instances. Built on `biota_http_server`'s own `Router`/`Client` (this
//! workspace hand-rolls its HTTP stack; reaching for axum/actix here would mean a second HTTP
//! implementation living beside the one already in this dependency graph) and `sangdb` for real,
//! disk-persisted state — a restarted control plane must not forget who was registered.

use crate::dto::{
    json_array, ConfigPushRequest, ConfigPushResult, HeartbeatRequest, InstanceView,
    RegisterRequest,
};
use crate::model::InstanceRecord;
use biota_http_server::actix::{Json, Responder};
use biota_http_server::{Client, Request, Response};
use sangdb::Db;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    db: Arc<RwLock<Db>>,
    data_dir: Arc<PathBuf>,
    http: Client,
}

impl AppState {
    pub async fn new(data_dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let data_dir = data_dir.into();
        let mut db = Db::new();
        // A fresh data dir has no `.seed` file yet — that's a first-run, not a real failure.
        match db.load::<InstanceRecord>(&data_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(AppState {
            db: Arc::new(RwLock::new(db)),
            data_dir: Arc::new(data_dir),
            http: Client::new(),
        })
    }

    async fn persist(&self) -> std::io::Result<()> {
        self.db.read().await.dump::<InstanceRecord>(&self.data_dir)
    }

    pub async fn register(&self, req: RegisterRequest, now_unix: i64) -> InstanceView {
        let record = InstanceRecord {
            id: req.id,
            addr: req.addr,
            version: req.version,
            healthy: 1,
            last_heartbeat_unix: now_unix,
            config_version: String::new(),
        };
        let stored = self.db.write().await.create(record).await;
        let _ = self.persist().await;
        InstanceView::from(&stored)
    }

    pub async fn heartbeat(&self, req: HeartbeatRequest, now_unix: i64) -> Option<InstanceView> {
        let existing = self.db.read().await.get::<InstanceRecord>(&req.id).await?;
        let updated = InstanceRecord {
            healthy: i64::from(req.healthy),
            last_heartbeat_unix: now_unix,
            config_version: req.config_version,
            ..existing
        };
        let stored = self.db.write().await.create(updated).await;
        let _ = self.persist().await;
        Some(InstanceView::from(&stored))
    }

    pub async fn list(&self) -> Vec<InstanceView> {
        self.db
            .read()
            .await
            .all::<InstanceRecord>()
            .await
            .iter()
            .map(InstanceView::from)
            .collect()
    }

    /// Push `config` to every instance matching `target` (`"*"` = all). Each push is a real HTTP
    /// POST via `self.http` to `{instance.addr}/relayfold-control/config` — failures (unreachable
    /// instance, non-2xx response) are reported per-instance, never silently dropped or allowed
    /// to abort the rest of the fleet's pushes.
    pub async fn push_config(&self, req: ConfigPushRequest) -> Vec<ConfigPushResult> {
        let all = self.db.read().await.all::<InstanceRecord>().await;
        let targets: Vec<InstanceRecord> = if req.target == "*" {
            all
        } else {
            all.into_iter().filter(|r| r.id == req.target).collect()
        };

        if targets.is_empty() {
            return vec![ConfigPushResult {
                id: req.target,
                ok: false,
                detail: "no matching instance registered".into(),
            }];
        }

        let mut results = Vec::with_capacity(targets.len());
        for instance in targets {
            let url = format!("{}/relayfold-control/config", instance.addr);
            let result = self
                .http
                .post(&url)
                .body(req.config.clone().into_bytes())
                .send()
                .await;
            let (ok, detail) = match result {
                Ok(resp) if resp.status().is_success() => (true, "delivered".to_string()),
                Ok(resp) => (false, format!("instance returned {}", resp.status())),
                Err(e) => (false, format!("delivery failed: {e}")),
            };
            results.push(ConfigPushResult {
                id: instance.id,
                ok,
                detail,
            });
        }
        results
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn handle_register(req: Request) -> Response {
    let Ok(Json(body)) = Json::<RegisterRequest>::from_request(&req) else {
        return Response::new(
            biota_http_server::StatusCode::BAD_REQUEST,
            "invalid register body",
        );
    };
    let Some(state) = req.extensions().get::<Arc<AppState>>().cloned() else {
        return Response::new(
            biota_http_server::StatusCode::INTERNAL_SERVER_ERROR,
            "state missing",
        );
    };
    let view = state.register(body, now_unix()).await;
    Json(view).respond_to(&req)
}

async fn handle_heartbeat(req: Request) -> Response {
    let Ok(Json(body)) = Json::<HeartbeatRequest>::from_request(&req) else {
        return Response::new(
            biota_http_server::StatusCode::BAD_REQUEST,
            "invalid heartbeat body",
        );
    };
    let Some(state) = req.extensions().get::<Arc<AppState>>().cloned() else {
        return Response::new(
            biota_http_server::StatusCode::INTERNAL_SERVER_ERROR,
            "state missing",
        );
    };
    match state.heartbeat(body, now_unix()).await {
        Some(view) => Json(view).respond_to(&req),
        None => Response::new(biota_http_server::StatusCode::NOT_FOUND, "no such instance"),
    }
}

async fn handle_list(req: Request) -> Response {
    let Some(state) = req.extensions().get::<Arc<AppState>>().cloned() else {
        return Response::new(
            biota_http_server::StatusCode::INTERNAL_SERVER_ERROR,
            "state missing",
        );
    };
    json_array(&state.list().await)
}

async fn handle_config_push(req: Request) -> Response {
    let Ok(Json(body)) = Json::<ConfigPushRequest>::from_request(&req) else {
        return Response::new(
            biota_http_server::StatusCode::BAD_REQUEST,
            "invalid config push body",
        );
    };
    let Some(state) = req.extensions().get::<Arc<AppState>>().cloned() else {
        return Response::new(
            biota_http_server::StatusCode::INTERNAL_SERVER_ERROR,
            "state missing",
        );
    };
    json_array(&state.push_config(body).await)
}

/// `state` is the raw `AppState`, not pre-wrapped — `Router::with_state` does its own
/// `Arc::new(state)` internally (see its doc: read back via `Arc<S>`); wrapping it again here
/// would inject `Arc<Arc<AppState>>` and every handler's `.get::<Arc<AppState>>()` would miss.
pub fn router(state: AppState) -> biota_http_server::Router {
    biota_http_server::Router::new()
        .post("/register", handle_register)
        .post("/heartbeat", handle_heartbeat)
        .get("/instances", handle_list)
        .post("/config/push", handle_config_push)
        .with_state(state)
}
