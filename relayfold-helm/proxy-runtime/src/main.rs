//! A real reverse-proxy binary over `biota-http-server`'s server half, routed by
//! `relayfold::Registry` from a config file that reloads without a restart.
//!
//! **What changed.** This used to build one `Proxy` from env vars, register every upstream
//! under the wildcard route `"*"`, and stop — so a customer adding a route or swapping a
//! backend pool needed a new pod. It now reads a YAML config ([`config`]), builds a
//! `relayfold::Registry` from its pools ([`routes`]), and reloads on SIGHUP or a config-file
//! change ([`reload`]), diffing the new config against the live table so only what actually
//! changed is patched.
//!
//! **What did not.** `biota-http-server`'s `Router`/`serve` still terminates HTTP and TLS and
//! serves the probe endpoints, its `Client` still does the forwarding, and the edge guards
//! ([`edge_guards`]) still run ahead of every request. This binary is the routing layer above
//! them, not a replacement for them.
//!
//! **Two environment variables, and only two**: `PROXY_CONFIG` (where the file is; default
//! `./proxy-runtime.yaml`) and `LISTEN_ADDR` (overrides the file's `listen`). Everything else
//! is in the file — a knob that exists in two places is a knob that will be set in one of
//! them and then be the wrong one.

use biota_http_server::{Request, Response, Router};
use relayfold_proxy_runtime::config::{self, ConfigError, ControlPlaneCfg, ProxyConfig};
use relayfold_proxy_runtime::edge_guards::{self as guards, AgentGate, Limits};
use relayfold_proxy_runtime::reload;
use relayfold_proxy_runtime::routes::RouteTable;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The live table. Reload replaces it under the write lock; a request clones it (cheap —
/// every field is an `Arc`) and drops the lock before touching the network, so a slow backend
/// can never delay the reload that would remove it.
type Shared = Arc<RwLock<RouteTable>>;

const DEFAULT_CONFIG_PATH: &str = "./proxy-runtime.yaml";

fn config_path() -> PathBuf {
    PathBuf::from(env::var("PROXY_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string()))
}

/// Read + parse + validate. The single gate every config enters through, at startup and on
/// every reload alike.
fn load_config(path: &Path) -> Result<ProxyConfig, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let mut cfg = config::parse(path, &text)?;
    cfg.validate()?;
    // The one override there is. Everything else lives in the file.
    if let Ok(addr) = env::var("LISTEN_ADDR") {
        if !addr.is_empty() {
            cfg.listen = Some(addr);
        }
    }
    Ok(cfg)
}

#[tokio::main]
async fn main() {
    let path = config_path();
    let cfg = match load_config(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[relayfold-proxy] cannot start: {e}");
            eprintln!(
                "[relayfold-proxy] set PROXY_CONFIG to a YAML config; see this image's README.md for the schema"
            );
            std::process::exit(2);
        }
    };

    let listen_addr = cfg
        .listen
        .clone()
        .unwrap_or_else(|| "0.0.0.0:8080".to_string());

    let tls = match &cfg.tls {
        Some(t) => {
            let chain = match std::fs::read_to_string(&t.cert_pem_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[relayfold-proxy] tls.cert_pem_path unreadable: {e}");
                    std::process::exit(2);
                }
            };
            let key = match std::fs::read_to_string(&t.key_pem_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[relayfold-proxy] tls.key_pem_path unreadable: {e}");
                    std::process::exit(2);
                }
            };
            Some(
                biota_http_server::tls_server_config(&chain, &key)
                    .expect("invalid TLS cert/key PEM"),
            )
        }
        None => None,
    };

    let table: Shared = Arc::new(RwLock::new(
        RouteTable::build(&cfg).expect("validated config builds a route table"),
    ));
    let last_good: Arc<RwLock<ProxyConfig>> = Arc::new(RwLock::new(cfg.clone()));

    println!(
        "[relayfold-proxy] listening on {listen_addr} (tls: {}, config: {}), routes {:?}",
        tls.is_some(),
        path.display(),
        table.read().unwrap().route_paths()
    );

    let limits = Limits::from_env(|k| env::var(k).ok());
    let gate = AgentGate::from_env(|k| env::var(k).ok());
    spawn_health_checks(Arc::clone(&table), &cfg);
    spawn_control_plane(
        Arc::clone(&table),
        Arc::clone(&last_good),
        cfg.control_plane.clone(),
    );
    spawn_reload(Arc::clone(&table), Arc::clone(&last_good), path.clone());

    let serving = Arc::clone(&table);
    let router = Router::new()
        // Probes are never agent-gated and never forwarded: a probe that answers 403 because
        // an agent is down is a false negative that restarts healthy pods.
        .get("/healthz", |_req: Request| async { Response::ok("ok") })
        .get("/readyz", |_req: Request| async { Response::ok("ready") })
        .fallback(move |mut req: Request| {
            // Clone the table, drop the lock, THEN run the guards and forward: the lock is a
            // table-swap lock, not a request lock.
            let snapshot = serving.read().unwrap().clone();
            let limits = limits;
            let gate = gate.clone();
            async move {
                if let Some(resp) = guards::guard(&mut req, &limits, &gate).await {
                    return resp;
                }
                snapshot.forward(req).await
            }
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

/// SIGHUP or a config-file change → re-read, re-validate, diff, swap. A config that fails to
/// parse or validate is reported and DISCARDED: the last-good table keeps serving, because a
/// typo must not take the proxy down.
fn spawn_reload(table: Shared, last_good: Arc<RwLock<ProxyConfig>>, path: PathBuf) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<reload::Trigger>();

    // Dropping a `JoinHandle` detaches the task; it keeps running for the life of the process,
    // which is what a signal listener should do.
    match reload::spawn_sighup(tx.clone()) {
        Ok(_handle) => println!("[relayfold-proxy] SIGHUP reload armed"),
        Err(e) => eprintln!("[relayfold-proxy] SIGHUP reload unavailable: {e}"),
    }
    reload::spawn_file_watch(path.clone(), reload::DEBOUNCE, tx);
    println!(
        "[relayfold-proxy] config watch armed ({}ms debounce)",
        reload::DEBOUNCE.as_millis()
    );

    tokio::spawn(async move {
        while let Some(trigger) = rx.recv().await {
            let cfg = match load_config(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "[relayfold-proxy] reload on {} FAILED, keeping the previous config: {e}",
                        trigger.label()
                    );
                    continue;
                }
            };
            let mut next = table.read().unwrap().clone();
            let diff = next.apply(&cfg);
            if diff.is_noop() {
                println!("[relayfold-proxy] reload on {}: no change", trigger.label());
                continue;
            }
            *table.write().unwrap() = next;
            // The control-plane refresher rewrites one pool of the live config; a file reload
            // supersedes it wholesale, so the refresher's baseline resets here.
            *last_good.write().unwrap() = cfg;
            println!(
                "[relayfold-proxy] reloaded on {}: {}",
                trigger.label(),
                diff.summary()
            );
            for (pool, out) in &diff.backends {
                println!(
                    "[relayfold-proxy]   pool {pool}: +{} -{} (~{} reweighted)",
                    out.added, out.removed, out.reweighted
                );
            }
        }
    });
}

/// Active health probing. Every backend in every pool is GETted on the configured path; the
/// verdict goes back through `Registry::set_health`, which is the store's own primitive.
fn spawn_health_checks(table: Shared, cfg: &ProxyConfig) {
    let Some(h) = cfg.health.clone() else {
        return;
    };
    let path = h.path.clone().unwrap_or_else(|| "/healthz".to_string());
    let interval = Duration::from_secs(h.interval_seconds.unwrap_or(10).max(1));
    let timeout = Duration::from_secs(h.timeout_seconds.unwrap_or(2).max(1));
    let client = biota_http_server::Client::new();
    println!(
        "[relayfold-proxy] health checks: GET {path} every {}s (timeout {}s)",
        interval.as_secs(),
        timeout.as_secs()
    );
    tokio::spawn(async move {
        loop {
            // Snapshot under the lock, probe WITHOUT it, then write back only if no reload
            // landed in between — otherwise the indices in the snapshot no longer line up with
            // the live backend set and a probe verdict would land on the wrong backend.
            let (generation, probes) = {
                let t = table.read().unwrap();
                (t.generation(), t.all_backends(&path))
            };
            let mut verdicts = Vec::with_capacity(probes.len());
            for (pool, idx, url) in &probes {
                let healthy = tokio::time::timeout(timeout, client.get(url).send())
                    .await
                    .map(|r| r.map(|resp| resp.status().is_success()).unwrap_or(false))
                    .unwrap_or(false);
                verdicts.push((pool.clone(), *idx, healthy));
            }
            // The guard is scoped to this block and dropped before the sleep below: an
            // RwLockReadGuard held across an await makes the whole future non-Send.
            {
                let t = table.read().unwrap();
                if t.generation() == generation {
                    for (pool, idx, healthy) in &verdicts {
                        t.set_health(pool, *idx, *healthy);
                    }
                } else {
                    eprintln!("[relayfold-proxy] discarding a health round that raced a reload");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// relayfold-control fleet source: poll `/instances`, and when the healthy address set
/// changes, rewrite ONLY the control-plane-owned pool's backends and push it through the same
/// diff a file reload uses. Every other pool belongs to the file alone.
fn spawn_control_plane(
    table: Shared,
    last_good: Arc<RwLock<ProxyConfig>>,
    cp: Option<ControlPlaneCfg>,
) {
    let Some(cp) = cp else { return };
    let base = cp.url.trim_end_matches('/').to_string();
    let pool = cp.pool.clone();
    let refresh = Duration::from_secs(cp.refresh_secs.unwrap_or(15).max(1));
    let client = biota_http_server::Client::new();
    println!(
        "[relayfold-proxy] control plane: {base}/instances -> pool {pool} every {}s",
        refresh.as_secs()
    );
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(refresh).await;
            let url = format!("{base}/instances");
            let body = match client.get(&url).send().await {
                Ok(resp) => resp.text().await.ok(),
                Err(e) => {
                    eprintln!(
                        "[relayfold-proxy] control plane unreachable at {url}: {e} — keeping current pool"
                    );
                    continue;
                }
            };
            let Some(body) = body else { continue };
            let Some(fresh) = healthy_addrs(&body) else {
                eprintln!("[relayfold-proxy] control plane answer was not an instances list");
                continue;
            };
            if fresh.is_empty() {
                eprintln!("[relayfold-proxy] control plane reachable but zero healthy instances — keeping current pool");
                continue;
            }
            let mut next_cfg = last_good.read().unwrap().clone();
            let Some(existing) = next_cfg.pools.get(&pool) else {
                eprintln!(
                    "[relayfold-proxy] control-plane pool {pool:?} is no longer in the config — not refreshing"
                );
                continue;
            };
            let mut entries = Vec::with_capacity(fresh.len());
            for url in &fresh {
                match config::pool_entry_of(url) {
                    Ok(e) => entries.push(e),
                    Err(e) => eprintln!(
                        "[relayfold-proxy] control plane reported an unusable address {url:?}: {e}"
                    ),
                }
            }
            if entries.is_empty() {
                continue;
            }
            let algo = existing.load_balance();
            next_cfg.pools.insert(
                pool.clone(),
                config::PoolCfg::Detailed {
                    load_balance: algo,
                    backends: entries,
                },
            );
            if let Err(e) = next_cfg.validate() {
                eprintln!("[relayfold-proxy] control-plane pool rejected by validation: {e}");
                continue;
            }
            let mut next = table.read().unwrap().clone();
            let diff = next.apply(&next_cfg);
            if !diff.is_noop() {
                *table.write().unwrap() = next;
                *last_good.write().unwrap() = next_cfg;
                eprintln!(
                    "[relayfold-proxy] pool {pool:?} refreshed from control plane: {}",
                    diff.summary()
                );
            }
        }
    });
}

/// `{"instances":[{"addr":"10.0.0.1:8080","healthy":true}, ...]}` → the healthy addresses,
/// with a scheme assumed where one is absent.
fn healthy_addrs(body: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = v.get("instances")?.as_array()?.clone();
    Some(
        arr.iter()
            .filter(|i| i.get("healthy").and_then(|h| h.as_bool()).unwrap_or(false))
            .filter_map(|i| i.get("addr")?.as_str().map(|s| s.to_string()))
            .map(|a| {
                if a.starts_with("http://") || a.starts_with("https://") {
                    a
                } else {
                    format!("http://{a}")
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_file_is_distinguishable_from_a_broken_one() {
        let missing = load_config(Path::new("/nonexistent/relayfold/proxy.yaml")).unwrap_err();
        assert!(missing.is_not_found(), "{missing}");
        let bad = scratch("bad-config.yaml");
        std::fs::write(
            &bad,
            "routes: [{match: {prefix: /}, upstream: missing}]\npools: {}\n",
        )
        .unwrap();
        let broken = load_config(&bad).unwrap_err();
        assert!(
            !broken.is_not_found(),
            "a file that exists but is invalid must not read as absent"
        );
    }

    #[test]
    fn healthy_addrs_filters_and_normalises() {
        let body = r#"{"instances":[
            {"addr":"10.0.0.1:8080","healthy":true},
            {"addr":"http://10.0.0.2:8080","healthy":true},
            {"addr":"10.0.0.3:8080","healthy":false}
        ]}"#;
        assert_eq!(
            healthy_addrs(body).unwrap(),
            vec!["http://10.0.0.1:8080", "http://10.0.0.2:8080"]
        );
        assert_eq!(healthy_addrs(r#"{"nope":1}"#), None);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(std::env::var("HOME").expect("HOME"))
            .join(".agent-work/scratch/relayfold-proxy-main-tests");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join(name)
    }
}
