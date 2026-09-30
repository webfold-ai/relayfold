//! A minimal, real reverse-proxy binary over `biota-http-server`'s own `Router`/`Proxy`.
//!
//! `biota-http-server` ships as a library only — nothing in the biota workspace instantiates a
//! `Router` and calls `serve()` as a standalone deployable service. `relayfold-helm`'s chart needs
//! a real container image to deploy, so this exists: `/healthz` and `/readyz` for Kubernetes
//! probes, and a catch-all fallback that forwards everything else through `Proxy::handle` to one
//! configured upstream (`Upstream::http`/`Upstream::https`, registered under the wildcard route
//! `"*"`, which `Proxy::pick`'s own fallback rule already treats as "any route with no more
//! specific match" — no new routing logic invented here).
//!
//! Config is env-var-only, on purpose: a container image's real config surface. No config file
//! parser here — `relayfold-chart-tool` (this repo's sibling crate) is where YAML config gets
//! parsed/validated, kept separate from this runtime binary.

use biota_http_server::{LoadBalance, Proxy, Request, Response, Router, Upstream};
use std::env;

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let listen_addr = env_or("LISTEN_ADDR", "0.0.0.0:8080");
    let upstream_host = env_or("UPSTREAM_HOST", "127.0.0.1");
    let upstream_port: u16 = env_or("UPSTREAM_PORT", "8081")
        .parse()
        .expect("UPSTREAM_PORT must be a valid u16");
    let upstream_scheme = env_or("UPSTREAM_SCHEME", "http");
    let lb_algo = match env_or("LOAD_BALANCE", "round_robin").as_str() {
        "random" => LoadBalance::Random,
        "weighted" => LoadBalance::Weighted,
        "consistent" => LoadBalance::Consistent,
        "healthiest" => LoadBalance::Healthiest,
        _ => LoadBalance::RoundRobin,
    };

    // Optional edge TLS: set both TLS_CERT_PEM_PATH + TLS_KEY_PEM_PATH and the
    // proxy terminates TLS itself on LISTEN_ADDR (fullchain PEM + PKCS8 key —
    // certbot's privkey.pem is exactly that). Unset = plain HTTP (loopback
    // behind a TLS front, or k8s where the ingress terminates). This is the
    // ptyktos-relay recipe's direct-ACME-edge shape: TLS at the node, no
    // tunnel, no third party in the data path.
    let tls = match (
        env::var("TLS_CERT_PEM_PATH").ok(),
        env::var("TLS_KEY_PEM_PATH").ok(),
    ) {
        (Some(cert_path), Some(key_path)) => {
            let chain = std::fs::read_to_string(&cert_path)
                .unwrap_or_else(|e| panic!("TLS_CERT_PEM_PATH unreadable: {e}"));
            let key = std::fs::read_to_string(&key_path)
                .unwrap_or_else(|e| panic!("TLS_KEY_PEM_PATH unreadable: {e}"));
            Some(biota_http_server::tls_server_config(&chain, &key)
                .expect("invalid TLS cert/key PEM"))
        }
        (None, None) => None,
        (cert, key) => panic!("TLS_CERT_PEM_PATH and TLS_KEY_PEM_PATH must be set together (got cert={cert:?}, key={key:?})"),
    };

    println!("relayfold-proxy listening on {listen_addr} (tls: {}), forwarding to {upstream_scheme}://{upstream_host}:{upstream_port}", tls.is_some());

    let upstream = if upstream_scheme == "https" {
        Upstream::https(upstream_host, upstream_port)
    } else {
        Upstream::http(upstream_host, upstream_port)
    };
    let proxy = Proxy::new().load_balance(lb_algo).upstream("*", upstream);

    let router = Router::new()
        .get("/healthz", |_req: Request| async { Response::ok("ok") })
        .get("/readyz", |_req: Request| async { Response::ok("ready") })
        .fallback(move |req: Request| {
            let proxy = proxy.clone();
            async move { proxy.handle(req).await }
        });

    match tls {
        Some(tls_config) => biota_http_server::serve_tls(listen_addr, router, tls_config)
            .await
            .expect("serve_tls failed"),
        None => biota_http_server::serve(listen_addr, router)
            .await
            .expect("serve failed"),
    }
}
