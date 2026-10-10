//! Shared helpers for the integration tests: a scratch dir under cargo's target tmpdir (not the
//! system temp dir), a real servelet-h1 server on an ephemeral loopback port, and a signing client.
#![allow(dead_code)]

use relayfold_control::auth::{public_key_of, Signer};
use relayfold_control::{router, serve_on, AppState, ControlConfig, ManualClock};
use servelet_client::Client;
use servelet_core::Response;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const START: i64 = 1_700_000_000;
// Labelled test dialkeys — literals for tests, never real credentials.
pub const DK_CONTROL: &str = "http-test-control-plane";
pub const DK_ADMIN: &str = "http-test-admin";
pub const DK_A: &str = "http-test-instance-a";
pub const DK_B: &str = "http-test-instance-b";

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn tempdir(tag: &str) -> TempDir {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "relayfold-control-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create scratch dir");
    TempDir(path)
}

pub fn config() -> ControlConfig {
    ControlConfig::new(DK_CONTROL, vec![public_key_of(DK_ADMIN)])
}

pub struct Server {
    pub base: String,
    pub clock: ManualClock,
    pub state: AppState,
    pub dir: TempDir,
}

/// Bind `127.0.0.1:0` *before* spawning, so the port is known and nothing races a sleep.
pub async fn spawn_on(router: servelet_router::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    assert_ne!(port, 0, "an ephemeral port must have been assigned");
    tokio::spawn(async move {
        let _ = serve_on(listener, router).await;
    });
    format!("http://127.0.0.1:{port}")
}

pub async fn spawn_control(cfg: ControlConfig) -> Server {
    let dir = tempdir("http");
    let clock = ManualClock::new(START);
    let state = AppState::open(dir.path(), cfg, Arc::new(clock.clone())).expect("AppState::open");
    let base = spawn_on(router(state.clone())).await;
    Server {
        base,
        clock,
        state,
        dir,
    }
}

pub struct Signed {
    pub path: &'static str,
    pub body: String,
    pub headers: [(&'static str, String); 4],
}

impl Signed {
    pub fn new(signer: &Signer, path: &'static str, body: &str, ts: i64) -> Signed {
        Signed {
            path,
            body: body.to_string(),
            headers: signer.sign_now("POST", path, body.as_bytes(), ts).pairs(),
        }
    }

    pub async fn send(&self, client: &Client, base: &str) -> Response {
        let mut rb = client
            .post(format!("{base}{}", self.path))
            .body(self.body.clone());
        for (name, value) in &self.headers {
            rb = rb.header(*name, value.clone());
        }
        rb.send().await.expect("request must complete")
    }
}

pub async fn post_signed(
    client: &Client,
    base: &str,
    signer: &Signer,
    path: &'static str,
    body: &str,
    ts: i64,
) -> Response {
    Signed::new(signer, path, body, ts).send(client, base).await
}

pub fn register_body(id: &str, addr: &str) -> String {
    format!(r#"{{"id":"{id}","addr":"{addr}","version":"0.1.0"}}"#)
}
