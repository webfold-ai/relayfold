//! Real, end-to-end tests: a real `servelet_h1` server on an ephemeral loopback port, a real
//! `servelet_client`, real Ed25519 signatures, a real config-receiving mock instance on its own
//! socket, and real `sangdb-log` persistence. The clock is injected (`ManualClock`) so expiry,
//! staleness and replay are exact, not slept.

mod common;

use common::*;
use wirefold::codec::syntax::{parse_surface, AsTerm, Atom, Surface, SyntaxTerm};
use relayfold_control::auth::{public_key_of, Signer};
use relayfold_control::config::verify_config;
use relayfold_control::{AppState, Clock, ControlPlane};
use servelet_client::Client;
use servelet_core::StatusCode;
use std::sync::Arc;
use tokio::sync::Mutex;

fn term_of(body: &str) -> SyntaxTerm {
    parse_surface(Surface::Json, body).expect("valid JSON response")
}

fn field<'a>(term: &'a SyntaxTerm, key: &str) -> &'a SyntaxTerm {
    let SyntaxTerm::Bu(fields) = term else {
        panic!("expected an object")
    };
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("missing field {key}"))
}

fn field_str<'a>(term: &'a SyntaxTerm, key: &str) -> &'a str {
    let SyntaxTerm::B(Atom::Str(s)) = field(term, key) else {
        panic!("field {key} is not a string")
    };
    s
}

/// The fleet as an enrolled viewer (`DK_A`) sees it; `ts` must be within the skew of the server
/// clock.
async fn instances(client: &Client, base: &str, ts: i64) -> Vec<SyntaxTerm> {
    let text = get_signed_instances(client, base, &Signer::from_dialkey(DK_A), ts)
        .await
        .text()
        .await
        .unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!("expected a JSON array, got {text}")
    };
    items
}

fn unauthorized() -> StatusCode {
    StatusCode::UNAUTHORIZED
}

// ── servelet serving ───────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn it_serves_over_servelet_h1_on_an_ephemeral_port() {
    let s = spawn_control(config()).await;
    // Not our own client: a raw TCP socket speaking HTTP/1.1 by hand, so what is asserted is the
    // wire bytes servelet-h1 puts on a real connection.
    let addr = s.base.trim_start_matches("http://").to_string();
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET /instances HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut raw),
    )
    .await
    .expect("server must answer within 5s")
    .unwrap();
    let raw = String::from_utf8_lossy(&raw).to_string();
    // `GET /instances` is no longer open: the unsigned request is a 401 on the wire.
    assert!(raw.starts_with("HTTP/1.1 401"), "status line: {raw}");
    assert!(
        raw.to_ascii_lowercase()
            .contains("www-authenticate: relayfold-dialkey"),
        "{raw}"
    );

    let client = Client::new();
    let signed = get_signed_instances(&client, &s.base, &Signer::from_dialkey(DK_A), START).await;
    assert_eq!(signed.status(), StatusCode::OK);
    assert!(signed
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json")));
    assert_eq!(
        signed.text().await.unwrap(),
        "[]",
        "an empty fleet is an empty JSON array"
    );

    let nf = client
        .get(format!("{}/no-such-route", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(nf.status(), StatusCode::NOT_FOUND);
}

// ── the original surface, now signed ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn register_then_list_shows_the_real_instance() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);

    let resp = post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-1", "http://127.0.0.1:9001"),
        START,
    )
    .await;
    assert!(
        resp.status().is_success(),
        "register must succeed, got {}",
        resp.status()
    );

    let items = instances(&client, &s.base, s.clock.now_unix()).await;
    assert_eq!(items.len(), 1);
    assert_eq!(field_str(&items[0], "id"), "proxy-1");
    assert_eq!(field_str(&items[0], "addr"), "http://127.0.0.1:9001");
    assert_eq!(
        field_str(&items[0], "public_key"),
        relayfold_control::auth::key_to_z32(&a.public_key()),
        "list must show the key the id is bound to"
    );
    assert_eq!(field(&items[0], "healthy"), &true.to_term());
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_updates_health_and_config_version() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);
    post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-2", "http://127.0.0.1:9002"),
        START,
    )
    .await;

    let hb = post_signed(
        &client,
        &s.base,
        &a,
        "/heartbeat",
        r#"{"id":"proxy-2","healthy":false,"config_version":"v7"}"#,
        START,
    )
    .await;
    assert!(hb.status().is_success());

    let items = instances(&client, &s.base, s.clock.now_unix()).await;
    assert_eq!(field_str(&items[0], "config_version"), "v7");
    assert_eq!(
        field(&items[0], "healthy"),
        &false.to_term(),
        "heartbeat(healthy=false) must actually flip the stored flag"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_for_an_unregistered_instance_is_a_real_404_not_a_silent_create() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);
    let hb = post_signed(
        &client,
        &s.base,
        &a,
        "/heartbeat",
        r#"{"id":"ghost","healthy":true,"config_version":"v1"}"#,
        START,
    )
    .await;
    assert_eq!(hb.status(), StatusCode::NOT_FOUND);
    assert!(
        instances(&client, &s.base, s.clock.now_unix())
            .await
            .is_empty(),
        "a heartbeat must never register"
    );
}

/// A real mock proxy instance: its own router on its own socket. It runs `verify_config` — the
/// function a relayfold agent would call — on whatever arrives, and records what it applied.
struct MockInstance {
    addr: String,
    applied: Arc<Mutex<Vec<(u64, String)>>>,
}

async fn spawn_mock_instance(
    id: &'static str,
    control_key: relayfold_control::auth::Key,
) -> MockInstance {
    let applied: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let log = applied.clone();
    let mock = servelet_router::Router::new().post(
        "/relayfold-control/config",
        move |req: servelet_core::Request| {
            let log = log.clone();
            async move {
                let mut log = log.lock().await;
                let last = log.last().map(|(v, _)| *v).unwrap_or(0);
                match verify_config(req.body(), &control_key, id, last) {
                    Ok(ok) => {
                        log.push((ok.config_version, ok.config));
                        servelet_core::Response::ok("applied")
                    }
                    Err(e @ relayfold_control::config::ConfigError::StaleVersion { .. }) => {
                        servelet_core::Response::new(
                            StatusCode::from_u16(409).unwrap(),
                            e.to_string(),
                        )
                    }
                    Err(e) => servelet_core::Response::new(StatusCode::UNAUTHORIZED, e.to_string()),
                }
            }
        },
    );
    MockInstance {
        addr: spawn_on(mock).await,
        applied,
    }
}

async fn push(
    client: &Client,
    s: &Server,
    signer: &Signer,
    target: &str,
    config: &str,
) -> servelet_core::Response {
    let body = format!(r#"{{"target":"{target}","config":"{config}"}}"#);
    post_signed(
        client,
        &s.base,
        signer,
        "/config/push",
        &body,
        s.clock_now(),
    )
    .await
}

impl Server {
    fn clock_now(&self) -> i64 {
        use relayfold_control::Clock;
        self.clock.now_unix()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn config_push_is_signed_delivered_and_verified_by_a_real_mock_instance() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let (a, admin) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_ADMIN));
    let control_key = public_key_of(DK_CONTROL);
    let mock = spawn_mock_instance("proxy-3", control_key).await;

    post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-3", &mock.addr),
        START,
    )
    .await;

    let resp = push(&client, &s, &admin, "proxy-3", "upstream: 10.0.0.5:8080").await;
    assert!(resp.status().is_success());
    let text = resp.text().await.unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!("expected an array")
    };
    assert_eq!(items.len(), 1);
    assert_eq!(
        field(&items[0], "ok"),
        &true.to_term(),
        "push report: {text}"
    );
    assert_eq!(field(&items[0], "config_version"), &1i64.to_term());

    assert_eq!(
        mock.applied.lock().await.clone(),
        vec![(1, "upstream: 10.0.0.5:8080".to_string())],
        "the mock instance must have received AND verified the config over a real socket"
    );

    // A second push gets a strictly newer version.
    push(&client, &s, &admin, "proxy-3", "upstream: 10.0.0.6:8080").await;
    let applied = mock.applied.lock().await.clone();
    assert_eq!(
        applied.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(s.state.config_version().await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_instance_rejects_unsigned_tampered_wrong_key_and_replayed_configs_over_the_wire() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let (a, admin) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_ADMIN));
    let mock = spawn_mock_instance("proxy-4", public_key_of(DK_CONTROL)).await;
    post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-4", &mock.addr),
        START,
    )
    .await;

    // Capture exactly what the control plane puts on the wire by pointing the registration at a
    // recorder first would change the addr; instead build the same envelope the plane builds.
    push(&client, &s, &admin, "proxy-4", "good: 1").await;
    assert_eq!(mock.applied.lock().await.len(), 1);
    let url = format!("{}/relayfold-control/config", mock.addr);

    let control = Signer::from_dialkey(DK_CONTROL);
    let valid_v2 = relayfold_control::config::sign_config(&control, "proxy-4", 2, "good: 2");

    // Unsigned: a bare config (what an unauthenticated push looked like before this change).
    let r = client.post(&url).body("good: 2").send().await.unwrap();
    assert_eq!(r.status(), unauthorized(), "bare config must be refused");
    // Wrong key.
    let imposter = relayfold_control::config::sign_config(
        &Signer::from_dialkey("http-test-imposter"),
        "proxy-4",
        2,
        "evil: 1",
    );
    let r = client.post(&url).body(imposter).send().await.unwrap();
    assert_eq!(
        r.status(),
        unauthorized(),
        "config signed by another key must be refused"
    );
    // Tampered: valid envelope with the config text swapped.
    let text = String::from_utf8(valid_v2.clone())
        .unwrap()
        .replace("good: 2", "evil: 2");
    let r = client.post(&url).body(text).send().await.unwrap();
    assert_eq!(
        r.status(),
        unauthorized(),
        "tampered config must be refused"
    );
    assert_eq!(
        mock.applied.lock().await.len(),
        1,
        "nothing above may have been applied"
    );

    // The genuine v2 applies once; replaying it, or the old v1, is refused with 409.
    let r = client
        .post(&url)
        .body(valid_v2.clone())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let r = client.post(&url).body(valid_v2).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 409, "replay of the applied version");
    let old_v1 = relayfold_control::config::sign_config(&control, "proxy-4", 1, "good: 1");
    let r = client.post(&url).body(old_v1).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 409, "replay of an old version");
    assert_eq!(mock.applied.lock().await.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn config_push_to_an_unknown_target_reports_failure_not_success() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let admin = Signer::from_dialkey(DK_ADMIN);
    let text = push(&client, &s, &admin, "nonexistent", "x")
        .await
        .text()
        .await
        .unwrap();
    let SyntaxTerm::Fourth(items) = term_of(&text) else {
        panic!()
    };
    assert_eq!(items.len(), 1);
    assert_eq!(field(&items[0], "ok"), &false.to_term());
    assert_eq!(
        s.state.config_version().await,
        0,
        "a push that matched nothing must not consume a version"
    );
}

// ── authentication, over the wire ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_unauthenticated_register_is_a_401_and_creates_nothing() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let resp = client
        .post(format!("{}/register", s.base))
        .body(register_body("proxy-x", "http://127.0.0.1:1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(instances(&client, &s.base, s.clock.now_unix())
        .await
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_signature_is_a_401() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    let body = register_body("proxy-x", "http://127.0.0.1:1");
    let mut req = Signed::new(&a, "/register", &body, START);
    // A's key and nonce, B's signature over the same bytes.
    let theirs = b.sign(
        "POST",
        "/register",
        body.as_bytes(),
        START,
        &req.headers[2].1,
    );
    req.headers[3].1 = theirs.signature;
    assert_eq!(
        req.send(&client, &s.base).await.status(),
        StatusCode::UNAUTHORIZED
    );
    // The same request body signed over a different body: tampering in transit.
    let mut tampered = Signed::new(&a, "/register", &body, START);
    tampered.body = register_body("proxy-evil", "http://127.0.0.1:6666");
    assert_eq!(
        tampered.send(&client, &s.base).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(instances(&client, &s.base, s.clock.now_unix())
        .await
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_signed_by_the_wrong_key_is_rejected_and_the_bound_key_still_works() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-5", "http://127.0.0.1:9005"),
        START,
    )
    .await;

    let hb = r#"{"id":"proxy-5","healthy":true,"config_version":"v1"}"#;
    let wrong = post_signed(&client, &s.base, &b, "/heartbeat", hb, START).await;
    assert_eq!(
        wrong.status(),
        StatusCode::FORBIDDEN,
        "valid signature, wrong key for this id"
    );
    let right = post_signed(&client, &s.base, &a, "/heartbeat", hb, START).await;
    assert!(right.status().is_success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_key_cannot_take_over_a_registered_id() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    let first = post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-6", "http://127.0.0.1:9006"),
        START,
    )
    .await;
    assert!(first.status().is_success());
    let takeover = post_signed(
        &client,
        &s.base,
        &b,
        "/register",
        &register_body("proxy-6", "http://evil.invalid:1"),
        START,
    )
    .await;
    assert_eq!(takeover.status(), StatusCode::FORBIDDEN);
    let items = instances(&client, &s.base, s.clock.now_unix()).await;
    assert_eq!(
        field_str(&items[0], "addr"),
        "http://127.0.0.1:9006",
        "address must be unchanged"
    );
    assert_eq!(
        field_str(&items[0], "public_key"),
        relayfold_control::auth::key_to_z32(&a.public_key())
    );
    // The owner re-registering (e.g. after moving) is fine.
    let again = post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-6", "http://127.0.0.1:9106"),
        START,
    )
    .await;
    assert!(again.status().is_success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_request_is_rejected() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);
    let req = Signed::new(
        &a,
        "/register",
        &register_body("proxy-7", "http://127.0.0.1:9007"),
        START,
    );
    assert!(req.send(&client, &s.base).await.status().is_success());
    // Byte-identical resend: same headers, same body.
    let again = req.send(&client, &s.base).await;
    assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_and_future_timestamps_are_rejected() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);
    let body = register_body("proxy-8", "http://127.0.0.1:9008");

    let stale = Signed::new(&a, "/register", &body, START);
    s.clock.advance(31);
    assert_eq!(
        stale.send(&client, &s.base).await.status(),
        StatusCode::UNAUTHORIZED,
        "31 s old, skew limit 30"
    );
    let future = Signed::new(&a, "/register", &body, START + 31 + 31);
    assert_eq!(
        future.send(&client, &s.base).await.status(),
        StatusCode::UNAUTHORIZED,
        "31 s ahead"
    );
    let fresh = Signed::new(&a, "/register", &body, START + 31);
    assert!(fresh.send(&client, &s.base).await.status().is_success());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_instance_that_stops_heartbeating_is_not_healthy_in_the_list() {
    let s = spawn_control(config()).await; // ttl 30 s
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);
    post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-9", "http://127.0.0.1:9009"),
        START,
    )
    .await;
    assert_eq!(
        field(
            &instances(&client, &s.base, s.clock.now_unix()).await[0],
            "healthy"
        ),
        &true.to_term()
    );

    s.clock.advance(29);
    assert_eq!(
        field(
            &instances(&client, &s.base, s.clock.now_unix()).await[0],
            "healthy"
        ),
        &true.to_term(),
        "29 s < ttl"
    );
    s.clock.advance(1);
    assert_eq!(
        field(
            &instances(&client, &s.base, s.clock.now_unix()).await[0],
            "healthy"
        ),
        &false.to_term(),
        "30 s == ttl: expired"
    );

    let hb = post_signed(
        &client,
        &s.base,
        &a,
        "/heartbeat",
        r#"{"id":"proxy-9","healthy":true,"config_version":"v1"}"#,
        START + 30,
    )
    .await;
    assert!(hb.status().is_success());
    assert_eq!(
        field(
            &instances(&client, &s.base, s.clock.now_unix()).await[0],
            "healthy"
        ),
        &true.to_term(),
        "a heartbeat revives it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_an_admin_key_may_push_config_and_nothing_is_delivered_otherwise() {
    let s = spawn_control(config()).await;
    let client = Client::new();
    let (a, admin) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_ADMIN));
    let mock = spawn_mock_instance("proxy-10", public_key_of(DK_CONTROL)).await;
    post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("proxy-10", &mock.addr),
        START,
    )
    .await;

    // Unauthenticated.
    let r = client
        .post(format!("{}/config/push", s.base))
        .body(r#"{"target":"*","config":"x"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // Authenticated as a registered instance, but not an admin.
    let r = push(&client, &s, &a, "*", "x").await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(mock.applied.lock().await.is_empty());
    assert_eq!(s.state.config_version().await, 0);
    // And the admin works.
    assert!(push(&client, &s, &admin, "*", "x")
        .await
        .status()
        .is_success());
    assert_eq!(mock.applied.lock().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_disk_is_a_500_and_nothing_is_applied() {
    struct FullDisk;
    impl relayfold_control::store::RecordSink for FullDisk {
        fn append(&mut self, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("no space left on device (injected)"))
        }
    }
    let clock = relayfold_control::ManualClock::new(START);
    let plane = ControlPlane::from_parts(
        config(),
        Arc::new(clock.clone()),
        relayfold_control::Stages::REAL,
        Default::default(),
        Box::new(FullDisk),
    );
    let state = AppState::from_plane(plane);
    let base = spawn_on(relayfold_control::router(state)).await;
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);
    let resp = post_signed(
        &client,
        &base,
        &a,
        "/register",
        &register_body("proxy-11", "http://127.0.0.1:9011"),
        START,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("device"),
        "internal error text must not leak to the caller: {body}"
    );
    assert!(
        instances(&client, &base, START).await.is_empty(),
        "a register whose append failed must not appear"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_state_survives_a_fresh_appstate_at_the_same_data_dir() {
    let dir = tempdir("restart");
    let clock = relayfold_control::ManualClock::new(START);
    let a = Signer::from_dialkey(DK_A);
    {
        let state = AppState::open(dir.path(), config(), Arc::new(clock.clone())).unwrap();
        let base = spawn_on(relayfold_control::router(state)).await;
        let client = Client::new();
        let r = post_signed(
            &client,
            &base,
            &a,
            "/register",
            &register_body("durable-1", "http://127.0.0.1:9999"),
            START,
        )
        .await;
        assert!(r.status().is_success());
    }
    // A brand-new AppState at the SAME dir, no shared in-memory state whatsoever.
    let state2 = AppState::open(dir.path(), config(), Arc::new(clock.clone())).unwrap();
    let list = state2.list().await;
    assert_eq!(
        list.len(),
        1,
        "a registered instance must survive a fresh AppState"
    );
    assert_eq!(list[0].id, "durable-1");
    assert_eq!(
        list[0].public_key,
        relayfold_control::auth::key_to_z32(&a.public_key())
    );
    assert!(
        !list[0].healthy,
        "not heard from since restart => not healthy until it heartbeats"
    );
}

// ── enrollment, the signed read path, compaction ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_unenrolled_key_cannot_register_and_the_refusal_is_logged_durably() {
    use relayfold_control::auth::key_to_z32;
    use relayfold_control::plane::NOT_ENROLLED;
    let s = spawn_control(config()).await;
    let client = Client::new();
    let outsider = Signer::from_dialkey(DK_OUTSIDER);

    let resp = post_signed(
        &client,
        &s.base,
        &outsider,
        "/register",
        &register_body("squatter", "http://127.0.0.1:6666"),
        START,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        resp.text().await.unwrap(),
        NOT_ENROLLED,
        "the refusal text is stable"
    );
    assert!(instances(&client, &s.base, START).await.is_empty());

    // An enrolled key registers the very same id: the namespace was never taken.
    let a = Signer::from_dialkey(DK_A);
    let ok = post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("squatter", "http://127.0.0.1:7777"),
        START,
    )
    .await;
    assert!(ok.status().is_success());

    // The refusal is in the durable log: a second reader of the same data dir sees it.
    let reread = ControlPlane::open(
        s.dir.path(),
        config(),
        Arc::new(s.clock.clone()),
        relayfold_control::Stages::REAL,
    )
    .unwrap();
    let rejections = reread.rejections();
    assert_eq!(rejections.len(), 1, "{rejections:?}");
    assert_eq!(rejections[0].id, "squatter");
    assert_eq!(
        key_to_z32(&rejections[0].key),
        key_to_z32(&outsider.public_key())
    );
    assert_eq!(rejections[0].at, START);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_instances_needs_a_signed_request_from_an_enrolled_key() {
    use relayfold_control::plane::NOT_ENROLLED;
    let s = spawn_control(config()).await;
    let client = Client::new();
    let (a, admin, outsider) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_ADMIN),
        Signer::from_dialkey(DK_OUTSIDER),
    );
    assert!(post_signed(
        &client,
        &s.base,
        &a,
        "/register",
        &register_body("p1", "http://127.0.0.1:9101"),
        START,
    )
    .await
    .status()
    .is_success());

    // No credentials: 401 with the challenge header.
    let r = client
        .get(format!("{}/instances", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // A forged signature: A's key, the outsider's signature.
    let mut forged = a.sign_now("GET", "/instances", b"", START);
    forged.signature = outsider
        .sign("GET", "/instances", b"", START, &forged.nonce)
        .signature;
    let mut rb = client.get(format!("{}/instances", s.base));
    for (n, v) in forged.pairs() {
        rb = rb.header(n, v);
    }
    assert_eq!(rb.send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // A signature A made for a different call is not a read credential.
    let hb = a.sign_now("POST", "/heartbeat", b"{}", START);
    let mut rb = client.get(format!("{}/instances", s.base));
    for (n, v) in hb.pairs() {
        rb = rb.header(n, v);
    }
    assert_eq!(rb.send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // Validly signed, not enrolled (an outsider, and the admin key): 403.
    for who in [&outsider, &admin] {
        let r = get_signed_instances(&client, &s.base, who, START).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(r.text().await.unwrap(), NOT_ENROLLED);
    }

    // Enrolled: 200 with the fleet; the identical bytes sent again are a replay: 401.
    let signed = a.sign_now("GET", "/instances", b"", START);
    let send = |h: [(&'static str, String); 4]| {
        let mut rb = client.get(format!("{}/instances", s.base));
        for (n, v) in h {
            rb = rb.header(n, v);
        }
        rb.send()
    };
    let first = send(signed.pairs()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert!(first.text().await.unwrap().contains("p1"));
    let again = send(signed.pairs()).await.unwrap();
    assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_keeps_the_fleet_and_the_binding_across_a_restart_over_http() {
    let dir = tempdir("compact-http");
    let clock = relayfold_control::ManualClock::new(START);
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    let before;
    {
        let state = AppState::open(dir.path(), config(), Arc::new(clock.clone())).unwrap();
        let base = spawn_on(relayfold_control::router(state.clone())).await;
        let client = Client::new();
        for (signer, id, addr) in [
            (&a, "c1", "http://127.0.0.1:9201"),
            (&b, "c2", "http://127.0.0.1:9202"),
            (&a, "c1", "http://127.0.0.1:9203"),
        ] {
            assert!(post_signed(
                &client,
                &base,
                signer,
                "/register",
                &register_body(id, addr),
                START
            )
            .await
            .status()
            .is_success());
        }
        let report = state.compact().await.unwrap();
        assert_eq!(report.records_folded, 3);
        assert!(report.log_bytes_after < report.log_bytes_before);
        before = instances(&client, &base, START).await.len();
        assert_eq!(before, 2);
    }
    let state = AppState::open(dir.path(), config(), Arc::new(clock.clone())).unwrap();
    let base = spawn_on(relayfold_control::router(state)).await;
    let client = Client::new();
    assert_eq!(instances(&client, &base, START).await.len(), before);
    // The binding survived the compaction: B cannot take over c1.
    let r = post_signed(
        &client,
        &base,
        &b,
        "/register",
        &register_body("c1", "http://127.0.0.1:6000"),
        START,
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}
