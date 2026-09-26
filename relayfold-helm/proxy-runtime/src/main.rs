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

    println!("relayfold-proxy listening on {listen_addr}, forwarding to {upstream_scheme}://{upstream_host}:{upstream_port}");

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

    biota_http_server::serve(listen_addr, router)
        .await
        .expect("serve failed");
}
