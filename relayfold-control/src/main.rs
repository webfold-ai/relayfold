//! Real binary entry point: `relayfold-control <bind-addr> <data-dir>`.
//!
//! * `RELAYFOLD_CONTROL_DIALKEY` (required) — the control plane's dialkey; the key derived from
//!   it signs every pushed config. Read from the environment, never printed.
//! * `RELAYFOLD_ADMIN_KEYS` — comma-separated z-base32 public keys allowed to call
//!   `/config/push`. Unset or empty means nobody may push.
//! * `RELAYFOLD_TTL_SECS` — heartbeat TTL (default 30).

use relayfold_control::auth::key_from_z32;
use relayfold_control::{AppState, ControlConfig, SystemClock};
use std::sync::Arc;

fn invalid(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
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
    let admin_keys = std::env::var("RELAYFOLD_ADMIN_KEYS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| key_from_z32(s).ok_or_else(|| invalid(format!("bad admin key {s:?}"))))
        .collect::<Result<Vec<_>, _>>()?;
    let mut cfg = ControlConfig::new(dialkey, admin_keys);
    if let Ok(ttl) = std::env::var("RELAYFOLD_TTL_SECS") {
        cfg.ttl_secs = ttl
            .parse()
            .map_err(|_| invalid(format!("bad RELAYFOLD_TTL_SECS {ttl:?}")))?;
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
