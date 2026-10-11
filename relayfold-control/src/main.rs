//! Real binary entry point: `relayfold-control <bind-addr> <data-dir>`.
//!
//! * `RELAYFOLD_CONTROL_DIALKEY` (required) — the control plane's dialkey; the key derived from
//!   it signs every pushed config. Read from the environment, never printed.
//! * `RELAYFOLD_ADMIN_KEYS` — comma-separated z-base32 public keys allowed to call
//!   `/config/push`. Unset or empty means nobody may push.
//! * `RELAYFOLD_ENROLLED_KEYS` — comma-separated z-base32 public keys allowed to `POST /register`
//!   and to `GET /instances`. Unset or empty means nobody may (a warning is printed).
//! * `RELAYFOLD_TTL_SECS` — heartbeat TTL (default 30).
//! * `RELAYFOLD_COMPACT_AFTER` — compact the durable log after this many appended records
//!   (default 10000; `0` disables automatic compaction).

use relayfold_control::auth::{key_from_z32, Key};
use relayfold_control::{AppState, ControlConfig, SystemClock};
use std::sync::Arc;

fn invalid(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
}

/// A comma-separated list of z-base32 public keys in `var` (unset = empty).
fn keys_from_env(var: &str, what: &str) -> std::io::Result<Vec<Key>> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| key_from_z32(s).ok_or_else(|| invalid(format!("bad {what} key {s:?} in {var}"))))
        .collect()
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "0.0.0.0:8088".to_string());
    let data_dir = args
        .next()
        .unwrap_or_else(|| "./relayfold-control-data".to_string());

    let dialkey = std::env::var("RELAYFOLD_CONTROL_DIALKEY")
        .map_err(|_| invalid("RELAYFOLD_CONTROL_DIALKEY must be set".into()))?;
    let admin_keys = keys_from_env("RELAYFOLD_ADMIN_KEYS", "admin")?;
    let enrolled = keys_from_env("RELAYFOLD_ENROLLED_KEYS", "enrolled")?;
    if enrolled.is_empty() {
        eprintln!(
            "relayfold-control: RELAYFOLD_ENROLLED_KEYS is empty: no instance can register \
             and nobody can list the fleet"
        );
    }
    let mut cfg = ControlConfig::new(dialkey, admin_keys).with_enrolled(enrolled);
    if let Ok(ttl) = std::env::var("RELAYFOLD_TTL_SECS") {
        cfg.ttl_secs = ttl
            .parse()
            .map_err(|_| invalid(format!("bad RELAYFOLD_TTL_SECS {ttl:?}")))?;
    }
    if let Ok(n) = std::env::var("RELAYFOLD_COMPACT_AFTER") {
        let n: u64 = n
            .parse()
            .map_err(|_| invalid(format!("bad RELAYFOLD_COMPACT_AFTER {n:?}")))?;
        cfg.compact_after = (n > 0).then_some(n);
    }

    let state = AppState::open(&data_dir, cfg, Arc::new(SystemClock))?;
    let router = relayfold_control::router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // The bound address, not the requested one: with `127.0.0.1:0` this is how a caller learns the port.
    println!(
        "relayfold-control listening on {}, data dir {data_dir}",
        listener.local_addr()?
    );
    relayfold_control::serve_on(listener, router).await
}
