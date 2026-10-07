//! Synthetic, end-to-end tests for the two edge guards — real TCP sockets, biota's real
//! `serve_connection` parser, the real `Proxy`, and the real `relayfold-agents` UDS protocol.
//! Nothing is stubbed except the thing each test is about (the upstream, the agent).

use biota_http_server::{LoadBalance, Proxy, Upstream};
use relayfold_agents::{Agent, AgentRequest, AgentRunner, Decision};
use relayfold_proxy_runtime::edge_guards::{build_router, AgentGate, Limits};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// -- harness ------------------------------------------------------------

/// Scratch sockets live under `~/projects/.agent-work/scratch/`, never `/tmp` (world-readable,
/// no cleanup, shows up in every `ls /tmp` on the box).
fn scratch_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    let dir = PathBuf::from(home).join(".agent-work/scratch/relayfold-proxy-tests");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A one-response upstream that always answers 200 — the "did the request get forwarded?"
/// oracle. If a guard returns anything other than 200, the request never reached this.
async fn spawn_upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nupstream")
                    .await;
            });
        }
    });
    port
}

/// Start the proxy's real router on a real socket and return its port.
async fn spawn_proxy(upstream_port: u16, gate: AgentGate) -> u16 {
    let mut proxy = Proxy::new().load_balance(LoadBalance::RoundRobin);
    proxy = proxy.upstream("*", Upstream::http("127.0.0.1", upstream_port));
    let shared = Arc::new(RwLock::new(proxy));
    let router = build_router(shared, Limits::default(), gate);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            let router = router.clone();
            tokio::spawn(biota_http_server::serve_connection(
                stream,
                router,
                Some(peer),
            ));
        }
    });
    port
}

/// Send raw bytes (so an oversized header really is oversized on the wire) and read the whole
/// response. `Connection: close` so the server closes after one response and `read_to_end`
/// terminates.
async fn raw_request(port: u16, request: &[u8]) -> String {
    let mut sock = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    sock.write_all(request).await.expect("write");
    sock.flush().await.expect("flush");
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(5), sock.read_to_string(&mut out))
        .await
        .expect("response within 5s")
        .expect("read");
    out
}

fn status_of(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

// -- 1. header limit ----------------------------------------------------

#[tokio::test]
async fn a_header_line_over_8kb_is_answered_400_not_forwarded_upstream() {
    let upstream_port = spawn_upstream().await;
    let proxy_port = spawn_proxy(upstream_port, AgentGate::disabled()).await;

    // Control: the same client, a small header — must be forwarded (200 from the upstream),
    // so the 400 below is attributable to size and nothing else.
    let ok = raw_request(
        proxy_port,
        b"GET /ok HTTP/1.1\r\nHost: x\r\nX-Small: v\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status_of(&ok), 200, "control request must forward: {ok}");

    // 16 KB header value — well over the 8 KB single-line limit.
    let big = "v".repeat(16 * 1024);
    let req = format!("GET / HTTP/1.1\r\nHost: x\r\nX-Huge: {big}\r\nConnection: close\r\n\r\n");
    let resp = raw_request(proxy_port, req.as_bytes()).await;

    assert_eq!(
        status_of(&resp),
        400,
        "a >8KB header must be 400 (nginx/haproxy parity), not a forwarded 200: {resp}"
    );
    // biota's HeaderMap lowercases header names, so the 400 names it as `x-huge`.
    assert!(
        resp.contains("x-huge"),
        "the 400 must name the offending header so an operator can act on it: {resp}"
    );
    assert!(
        resp.contains("16394 bytes") && resp.contains("8192"),
        "the 400 must carry the observed and permitted sizes: {resp}"
    );
    assert!(
        !resp.contains("upstream"),
        "the oversized request must never reach the upstream: {resp}"
    );
}

#[tokio::test]
async fn many_small_headers_totalling_over_the_block_limit_are_also_400() {
    let upstream_port = spawn_upstream().await;
    let proxy_port = spawn_proxy(upstream_port, AgentGate::disabled()).await;

    // 40 headers x 1 KB each = ~40 KB of header block, every line well under the 8 KB
    // per-line limit and every line legal. The per-line check alone passes this straight
    // through; the total-block cap (4 x 8 KB, nginx's `large_client_header_buffers 4 8k`)
    // is what refuses it.
    let mut req = String::from("GET / HTTP/1.1\r\nHost: x\r\n");
    let chunk = "v".repeat(1000);
    for i in 0..40 {
        req.push_str(&format!("x-pad-{i}: {chunk}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    let resp = raw_request(proxy_port, req.as_bytes()).await;
    assert_eq!(
        status_of(&resp),
        400,
        "oversized header block must be 400: {resp}"
    );
}

// -- 2. agent gate ------------------------------------------------------

#[derive(Clone)]
struct DenyBlockedPaths;

impl Agent for DenyBlockedPaths {
    async fn on_call(&self, request: &AgentRequest) -> Decision {
        if request.path.starts_with("/blocked") {
            Decision::deny()
                .with_status(403)
                .with_body("blocked by agent")
                .build()
        } else {
            Decision::Allow
        }
    }
}

/// Start a real agent process-equivalent: a real `AgentRunner` on a real UDS socket.
async fn spawn_agent(name: &str) -> PathBuf {
    let socket = scratch_dir().join(format!("{name}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let runner = AgentRunner::new(DenyBlockedPaths).with_socket(&socket);
    let handle = tokio::spawn(runner.serve());
    // Real startup race: the listener is not necessarily bound yet.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cleanup = socket.clone();
    tokio::spawn(async move {
        handle.await.ok();
        let _ = std::fs::remove_file(cleanup);
    });
    socket
}

#[tokio::test]
async fn an_agent_returning_deny_gets_its_configured_response_and_no_upstream_call() {
    let upstream_port = spawn_upstream().await;
    let socket = spawn_agent("deny").await;
    let proxy_port = spawn_proxy(
        upstream_port,
        AgentGate::new(socket, Duration::from_secs(2)),
    )
    .await;

    let denied = raw_request(
        proxy_port,
        b"GET /blocked/secret HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status_of(&denied), 403, "a Deny must answer 403: {denied}");
    assert!(
        denied.contains("blocked by agent"),
        "the agent's own body must reach the client: {denied}"
    );
    assert!(
        !denied.contains("upstream"),
        "a denied request must never reach the upstream: {denied}"
    );

    // Same agent, allowed path — proves the gate is a real per-request consultation and the
    // proxy still forwards when the agent says Allow.
    let allowed = raw_request(
        proxy_port,
        b"GET /fine HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status_of(&allowed), 200, "Allow must forward: {allowed}");
}

#[tokio::test]
async fn an_unreachable_agent_fails_open_and_traffic_still_flows() {
    let upstream_port = spawn_upstream().await;
    // A socket path nothing is listening on — the agent process is down.
    let dead = scratch_dir().join("agent-that-was-never-started.sock");
    let _ = std::fs::remove_file(&dead);
    let proxy_port =
        spawn_proxy(upstream_port, AgentGate::new(&dead, Duration::from_secs(2))).await;

    let resp = raw_request(
        proxy_port,
        b"GET /blocked/secret HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        status_of(&resp),
        200,
        "the proxy must fail open when its agent is down: {resp}"
    );
}

#[tokio::test]
async fn the_healthz_probe_is_never_agent_gated() {
    let upstream_port = spawn_upstream().await;
    let dead = scratch_dir().join("agent-that-was-never-started-2.sock");
    let proxy_port = spawn_proxy(upstream_port, AgentGate::new(dead, Duration::from_secs(2))).await;
    let resp = raw_request(
        proxy_port,
        b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        status_of(&resp),
        200,
        "a probe must answer even when the agent is down, or a healthy pod gets restarted: {resp}"
    );
}
