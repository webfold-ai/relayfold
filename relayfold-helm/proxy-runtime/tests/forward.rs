//! The forwarder against real servelet servers on loopback ports: what reaches the upstream, what
//! comes back, how requests spread across a pool, and what a down upstream does.

use std::time::Duration;

use relayfold::LoadBalance;
use relayfold_proxy_runtime::Proxy;
use servelet_client::Client;
use servelet_core::{Method, Request, Response, StatusCode};
use servelet_router::Router;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn serve_on(port: u16, router: Router) {
    tokio::spawn(servelet_h1::serve(("127.0.0.1", port), router));
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server on {port} never came up");
}

/// An upstream that answers every request with `id`, and `/healthz` with `health`.
async fn labelled_upstream(id: &'static str, health: u16) -> String {
    let port = free_port();
    let router = Router::new()
        .get("/healthz", move |_req: Request| async move {
            Response::new(StatusCode::from_u16(health).unwrap(), "h")
        })
        .fallback(move |_req: Request| async move { Response::ok(id) });
    serve_on(port, router).await;
    format!("http://127.0.0.1:{port}")
}

async fn proxy_in_front(proxy: Proxy) -> String {
    let port = free_port();
    let router = Router::new().fallback(move |req: Request| {
        let proxy = proxy.clone();
        async move { proxy.handle(req).await }
    });
    serve_on(port, router).await;
    format!("http://127.0.0.1:{port}")
}

async fn body_of(client: &Client, url: &str) -> String {
    client.get(url).send().await.unwrap().text().await.unwrap()
}

#[tokio::test]
async fn forwards_method_path_query_headers_and_body_and_returns_status_and_headers() {
    let port = free_port();
    let router = Router::new().fallback(|req: Request| async move {
        let seen = format!(
            "{} {} body={} x-test={} xff={}",
            req.method().as_str(),
            req.uri().path_and_query(),
            String::from_utf8_lossy(req.body()),
            req.headers()
                .get("x-test")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-"),
            req.headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-"),
        );
        Response::new(StatusCode::from_u16(201).unwrap(), seen).with_header("x-upstream", "yes")
    });
    serve_on(port, router).await;

    let proxy = Proxy::new(
        LoadBalance::RoundRobin,
        &[format!("http://127.0.0.1:{port}")],
    );
    let front = proxy_in_front(proxy).await;

    let resp = Client::new()
        .request(Method::POST, format!("{front}/a/b?q=1&r=two"))
        .header("x-test", "hello")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status.as_u16(),
        201,
        "the upstream's status passes through"
    );
    assert_eq!(
        resp.headers.get("x-upstream").and_then(|v| v.to_str().ok()),
        Some("yes")
    );
    let text = resp.text().await.unwrap();
    assert!(
        text.starts_with("POST /a/b?q=1&r=two body=payload x-test=hello xff="),
        "got: {text}"
    );
    assert!(
        text.contains("xff=127.0.0.1"),
        "the caller's address is appended to X-Forwarded-For: {text}"
    );
}

#[tokio::test]
async fn round_robin_spreads_requests_across_the_pool() {
    let a = labelled_upstream("A", 200).await;
    let b = labelled_upstream("B", 200).await;
    let front = proxy_in_front(Proxy::new(LoadBalance::RoundRobin, &[a, b])).await;
    let client = Client::new();
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(body_of(&client, &format!("{front}/x")).await);
    }
    assert_eq!(seen.iter().filter(|s| *s == "A").count(), 3, "{seen:?}");
    assert_eq!(seen.iter().filter(|s| *s == "B").count(), 3, "{seen:?}");
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502_not_a_hang() {
    let dead = format!("http://127.0.0.1:{}", free_port());
    let front = proxy_in_front(
        Proxy::new(LoadBalance::RoundRobin, &[dead]).with_timeout(Duration::from_secs(5)),
    )
    .await;
    let resp = Client::new()
        .get(format!("{front}/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 502);
}

#[tokio::test]
async fn no_upstreams_is_a_503() {
    let front = proxy_in_front(Proxy::new(LoadBalance::RoundRobin, &[])).await;
    let resp = Client::new()
        .get(format!("{front}/x"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status.as_u16(), 503);
}

#[tokio::test]
async fn health_checks_take_a_failing_upstream_out_of_rotation() {
    let sick = labelled_upstream("SICK", 500).await;
    let well = labelled_upstream("WELL", 200).await;
    let proxy = Proxy::new(LoadBalance::RoundRobin, &[sick, well]);
    let probes = proxy.spawn_health_checks("/healthz".to_string(), Duration::from_millis(100));
    tokio::time::sleep(Duration::from_millis(600)).await;
    let front = proxy_in_front(proxy).await;
    let client = Client::new();
    for _ in 0..6 {
        assert_eq!(body_of(&client, &format!("{front}/x")).await, "WELL");
    }
    probes.abort();
}

#[tokio::test]
async fn headers_named_by_connection_are_not_forwarded_either_way() {
    let port = free_port();
    let router = Router::new().fallback(|req: Request| async move {
        let h = |n: &str| {
            req.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_string()
        };
        let seen = format!("local={} keep={}", h("x-session-state"), h("x-keep"));
        Response::ok(seen)
            .with_header("connection", "X-Up-Local")
            .with_header("x-up-local", "secret")
            .with_header("x-up-keep", "yes")
    });
    serve_on(port, router).await;
    let front = proxy_in_front(Proxy::new(
        LoadBalance::RoundRobin,
        &[format!("http://127.0.0.1:{port}")],
    ))
    .await;

    let resp = Client::new()
        .get(format!("{front}/x"))
        .header("connection", "x-session-state")
        .header("x-session-state", "abc")
        .header("x-keep", "yes")
        .send()
        .await
        .unwrap();
    assert!(
        resp.headers.get("x-up-local").is_none(),
        "a response field named by Connection is dropped"
    );
    assert_eq!(
        resp.headers.get("x-up-keep").and_then(|v| v.to_str().ok()),
        Some("yes")
    );
    let text = resp.text().await.unwrap();
    assert_eq!(
        text, "local=- keep=yes",
        "a request field named by Connection is dropped, the rest forwarded"
    );
}
