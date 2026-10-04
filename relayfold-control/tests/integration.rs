//! Real, end-to-end tests: no mocked `Db`, no mocked HTTP — a real `servelet_h1::serve`
//! instance, a real config-receiving mock instance on a real socket, real `sangdb` disk
//! persistence across a fresh `AppState`.

use hashtrinity::codec::syntax::{parse_surface, AsTerm, Surface, SyntaxTerm};
use relayfold_control::{router, AppState};
use std::sync::atomic::{AtomicU16, Ordering};

static NEXT_PORT: AtomicU16 = AtomicU16::new(28080);

fn next_addr() -> (String, u16) {
    let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
    (format!("127.0.0.1:{port}"), port)
}

async fn spawn_control_plane(data_dir: &std::path::Path) -> String {
    let (addr, port) = next_addr();
    let state = AppState::new(data_dir).await.expect("AppState::new");
    let r = router(state);
    tokio::spawn(async move {
        let _ = servelet_h1::serve(&addr, r).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    format!("http://127.0.0.1:{port}")
}

fn term_of(body: &str) -> SyntaxTerm {
    parse_surface(Surface::Json, body).expect("valid JSON response")
}

fn field_str<'a>(term: &'a SyntaxTerm, key: &str) -> &'a str {
    let SyntaxTerm::Bu(fields) = term else {
        panic!("expected an object")
    };
    let SyntaxTerm::B(hashtrinity::codec::syntax::Atom::Str(s)) = fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("missing field {key}"))
    else {
        panic!("field {key} is not a string")
    };
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn register_then_list_shows_the_real_instance() {
    let dir = tempdir();
    let base = spawn_control_plane(dir.path()).await;
    let client = servelet_client::Client::new();

    let resp = client
        .post(format!("{base}/register"))
        .body(r#"{"id":"proxy-1","addr":"http://127.0.0.1:9001","version":"0.1.0"}"#)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "register must succeed, got {}",
        resp.status()
    );

    let list = client
        .get(format!("{base}/instances"))
        .send()
        .await
        .unwrap();
    assert!(list.status().is_success());
    let text = list.text().await.unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!("expected a JSON array")
    };
    assert_eq!(
        items.len(),
        1,
        "one registered instance should appear, got: {text}"
    );
    assert_eq!(field_str(&items[0], "id"), "proxy-1");
    assert_eq!(field_str(&items[0], "addr"), "http://127.0.0.1:9001");
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_updates_health_and_config_version() {
    let dir = tempdir();
    let base = spawn_control_plane(dir.path()).await;
    let client = servelet_client::Client::new();

    client
        .post(format!("{base}/register"))
        .body(r#"{"id":"proxy-2","addr":"http://127.0.0.1:9002","version":"0.1.0"}"#)
        .send()
        .await
        .unwrap();

    let hb = client
        .post(format!("{base}/heartbeat"))
        .body(r#"{"id":"proxy-2","healthy":false,"config_version":"v7"}"#)
        .send()
        .await
        .unwrap();
    assert!(hb.status().is_success());

    let list = client
        .get(format!("{base}/instances"))
        .send()
        .await
        .unwrap();
    let text = list.text().await.unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!("expected an array")
    };
    assert_eq!(field_str(&items[0], "config_version"), "v7");
    let SyntaxTerm::Bu(fields) = &items[0] else {
        panic!()
    };
    let healthy = fields
        .iter()
        .find(|(k, _)| k == "healthy")
        .map(|(_, v)| v.clone());
    assert_eq!(
        healthy,
        Some(false.to_term()),
        "heartbeat(healthy=false) must actually flip the stored flag"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_for_an_unregistered_instance_is_a_real_404_not_a_silent_create() {
    let dir = tempdir();
    let base = spawn_control_plane(dir.path()).await;
    let client = servelet_client::Client::new();

    let hb = client
        .post(format!("{base}/heartbeat"))
        .body(r#"{"id":"ghost","healthy":true,"config_version":"v1"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(hb.status(), servelet_core::StatusCode::NOT_FOUND);

    let list = client
        .get(format!("{base}/instances"))
        .send()
        .await
        .unwrap();
    let text = list.text().await.unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!()
    };
    assert!(
        items.is_empty(),
        "a heartbeat for an unregistered id must never silently register it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn config_push_actually_delivers_to_a_real_mock_instance() {
    let dir = tempdir();
    let base = spawn_control_plane(dir.path()).await;
    let client = servelet_client::Client::new();

    // A real mock proxy instance: its own servelet_router::Router, on its own real socket,
    // recording whatever config body it actually receives.
    let received: std::sync::Arc<tokio::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let (mock_addr, mock_port) = next_addr();
    let received_clone = received.clone();
    let mock_router = servelet_router::Router::new().post(
        "/relayfold-control/config",
        move |req: servelet_core::Request| {
            let received = received_clone.clone();
            async move {
                let body = String::from_utf8_lossy(req.body()).to_string();
                *received.lock().await = Some(body);
                servelet_core::Response::ok("applied")
            }
        },
    );
    tokio::spawn(async move {
        let _ = servelet_h1::serve(&mock_addr, mock_router).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;

    client
        .post(format!("{base}/register"))
        .body(format!(
            r#"{{"id":"proxy-3","addr":"http://127.0.0.1:{mock_port}","version":"0.1.0"}}"#
        ))
        .send()
        .await
        .unwrap();

    let push = client
        .post(format!("{base}/config/push"))
        .body(r#"{"target":"proxy-3","config":"upstream: 10.0.0.5:8080"}"#)
        .send()
        .await
        .unwrap();
    assert!(push.status().is_success());
    let text = push.text().await.unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!("expected an array")
    };
    assert_eq!(items.len(), 1);
    let SyntaxTerm::Bu(fields) = &items[0] else {
        panic!()
    };
    let ok = fields
        .iter()
        .find(|(k, _)| k == "ok")
        .map(|(_, v)| v.clone());
    assert_eq!(ok, Some(true.to_term()), "push report: {text}");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let got = received.lock().await.clone();
    assert_eq!(
        got.as_deref(),
        Some("upstream: 10.0.0.5:8080"),
        "the mock instance must have actually received the pushed config over a real socket"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn config_push_to_an_unknown_target_reports_failure_not_success() {
    let dir = tempdir();
    let base = spawn_control_plane(dir.path()).await;
    let client = servelet_client::Client::new();

    let push = client
        .post(format!("{base}/config/push"))
        .body(r#"{"target":"nonexistent","config":"x"}"#)
        .send()
        .await
        .unwrap();
    let text = push.text().await.unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!()
    };
    assert_eq!(items.len(), 1);
    let SyntaxTerm::Bu(fields) = &items[0] else {
        panic!()
    };
    let ok = fields
        .iter()
        .find(|(k, _)| k == "ok")
        .map(|(_, v)| v.clone());
    assert_eq!(ok, Some(false.to_term()), "pushing to a target that matches no registered instance must report failure, not silently succeed");
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_state_survives_a_fresh_appstate_at_the_same_data_dir() {
    let dir = tempdir();
    {
        let state = AppState::new(dir.path()).await.unwrap();
        let req = relayfold_control::dto::RegisterRequest {
            id: "durable-1".into(),
            addr: "http://127.0.0.1:9999".into(),
            version: "1.2.3".into(),
        };
        state.register(req, 1_000_000).await;
    }
    // A brand-new AppState at the SAME dir, no shared in-memory state whatsoever.
    let state2 = AppState::new(dir.path()).await.unwrap();
    let list = state2.list().await;
    assert_eq!(
        list.len(),
        1,
        "a registered instance must survive a fresh AppState at the same data dir"
    );
    assert_eq!(list[0].id, "durable-1");
    assert_eq!(list[0].version, "1.2.3");
}

fn tempdir() -> TempDir {
    let path = std::env::temp_dir()
        .join(format!("relayfold-control-test-{}", std::process::id()))
        .join(format!("{:?}", std::time::SystemTime::now()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir(path)
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
