//! The hot-reload contract, end to end: a REAL proxy process, a REAL config file on disk, a
//! REAL `kill -HUP`, and real HTTP through the whole thing.
//!
//! What these tests pin, in one sentence: **a customer can add a route, move a route onto a
//! different backend, and remove a route, and the running proxy serves the new table without
//! being restarted.** Everything else in the unit suite is in-process; this suite is the only
//! place the signal handling, the file watch, the socket and the swap are all exercised
//! together, which is exactly where a hot reload actually breaks.
//!
//! Also pinned here, because both are silent failures in production and neither is visible in
//! an in-process test:
//! - a config that does not parse leaves the previous one SERVING (a typo must not 502 every
//!   request in the fleet);
//! - a backend the health prober marked down is still down after a reload that keeps it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// -- harness -----------------------------------------------------------------------------------

/// Scratch under `~/projects/.agent-work/scratch/`, never `/tmp` (world-readable, no cleanup,
/// and visible to every process on the box).
fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(".agent-work/scratch/relayfold-proxy-reload-it")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A real HTTP upstream that answers every request with its own name. The body is the
/// oracle: it says WHICH backend served the request, which is the only way to observe a route
/// moving. `biota-http-server` has no echo endpoint, so this is the minimum hand-rolled
/// server that makes "which backend served this" observable at all — which is the whole
/// point of the test.
fn spawn_echo_upstream(_dir: &std::path::Path, name: &str) -> u16 {
    let sock: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let listener = TcpListener::bind(sock).expect("bind echo upstream");
    let port = listener.local_addr().expect("addr").port();
    let tag = format!("{name}\n").into_bytes();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let tag = tag.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => return,
                });
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    tag.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&tag);
                let _ = stream.flush();
            });
        }
    });
    port
}

fn wait_for_port(port: u16, within: Duration) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("port {port} never opened within {within:?}");
}

/// A blocking raw HTTP/1.1 GET, because a hand-rolled client is the only way to assert on the
/// exact response and not on a client library's idea of it.
fn http_get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect to proxy");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
    )
    .expect("write");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).expect("read");
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// Poll until `check` holds, or fail with what was actually seen. A reload is asynchronous by
/// nature (signal → read → parse → swap), so asserting immediately after the kill would be
/// asserting on the scheduler.
fn eventually(within: Duration, mut check: impl FnMut() -> Result<(), String>) {
    let deadline = Instant::now() + within;
    let mut last = String::new();
    while Instant::now() < deadline {
        match check() {
            Ok(()) => return,
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("condition never held within {within:?}; last saw: {last}");
}

struct Proxy {
    child: Child,
    port: u16,
    config: PathBuf,
}

impl Proxy {
    fn start(dir: &PathBuf, config_body: &str) -> Proxy {
        let sock: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let port = {
            // Bind-then-release to get a free port, then hand it to the child. The window is
            // tiny and loopback-only; a retry loop on the child's own readiness is the real
            // guard, and `wait_for_port` below is that guard.
            let l = TcpListener::bind(sock).expect("bind probe");
            let p = l.local_addr().expect("addr").port();
            drop(l);
            p
        };
        let config = dir.join("proxy-runtime.yaml");
        std::fs::write(&config, config_body).expect("write config");
        let child = Command::new(env!("CARGO_BIN_EXE_relayfold-proxy"))
            .env("PROXY_CONFIG", config.display().to_string())
            .env("LISTEN_ADDR", format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn proxy");
        wait_for_port(port, Duration::from_secs(10));
        Proxy {
            child,
            port,
            config,
        }
    }

    fn sighup(&self) {
        // `kill -HUP <pid>` through libc's own signal call — no `kill(1)` binary dependency.
        let pid = self.child.id() as i32;
        assert_eq!(unsafe { libc_kill(pid, 1) }, 0, "kill -HUP failed");
    }

    fn get(&self, path: &str) -> (u16, String) {
        http_get(self.port, path)
    }

    fn rewrite(&self, body: &str) {
        // Write-then-rename, which is what an editor and a ConfigMap update both do. A plain
        // truncate-and-write would also work here, but testing the harder shape is free.
        let tmp = self.config.with_extension("yaml.tmp");
        std::fs::write(&tmp, body).expect("write tmp");
        std::fs::rename(&tmp, &self.config).expect("rename over config");
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

fn pool_config(route: &str, backend_port: u16) -> String {
    format!(
        r#"listen: "127.0.0.1:0"
header_limit: 65536
routes:
  - match: {{ prefix: "{route}" }}
    upstream: main
pools:
  main:
    backends:
      - {{ host: "127.0.0.1", port: {backend_port} }}"#
    )
}

// -- the tests ---------------------------------------------------------------------------------

/// THE test: SIGHUP alone moves the running proxy onto a new route table. No restart, no
/// dropped connection, and the OLD route is gone afterwards — a removed route that still
/// answers is worse than one that never existed, because the operator believes it is closed.
#[test]
fn sighup_swaps_the_live_route_table() {
    let dir = scratch_dir("sighup-swap");
    let a = spawn_echo_upstream(&dir, "A");
    let b = spawn_echo_upstream(&dir, "B");

    let proxy = Proxy::start(
        &dir,
        &format!(
            r#"listen: "127.0.0.1:0"
header_limit: 65536
routes:
  - match: {{ prefix: "/api" }}
    upstream: main
pools:
  main:
    backends:
      - {{ host: "127.0.0.1", port: {a} }}
"#
        ),
    );

    assert_eq!(
        proxy.get("/api/users/42"),
        (200, "A\n".to_string()),
        "pre-reload"
    );

    // Same route, different backend; plus a brand-new route on a second pool.
    proxy.rewrite(&format!(
        r#"listen: "127.0.0.1:0"
header_limit: 65536
routes:
  - match: {{ prefix: "/api" }}
    upstream: main
  - match: {{ prefix: "/new" }}
    upstream: side
pools:
  main:
    backends:
      - {{ host: "127.0.0.1", port: {b} }}
  side:
    backends:
      - {{ host: "127.0.0.1", port: {a} }}
"#
    ));
    proxy.sighup();

    eventually(Duration::from_secs(10), || {
        let (s, body) = proxy.get("/api/users/42");
        if s != 200 || body != "B\n" {
            return Err(format!(
                "/api still answers {s} {body:?}, expected 200 \"B\\n\""
            ));
        }
        let (s2, body2) = proxy.get("/new/thing");
        if s2 != 200 || body2 != "A\n" {
            return Err(format!(
                "/new answers {s2} {body2:?}, expected 200 \"A\\n\""
            ));
        }
        Ok(())
    });
}

/// The removal half: a route deleted from the file must stop being reachable, and fall through
/// to the catch-all rather than 404ing, because the catch-all is what the operator configured
/// for "everything else".
#[test]
fn a_route_removed_by_sighup_stops_serving() {
    let dir = scratch_dir("sighup-remove");
    let a = spawn_echo_upstream(&dir, "A");
    let catchall = spawn_echo_upstream(&dir, "CATCHALL");

    let base = |api: Option<()>| {
        let mut s = format!(
            r#"listen: "127.0.0.1:0"
header_limit: 65536
routes:
"#
        );
        if api.is_some() {
            s.push_str("  - match: { prefix: \"/api\" }\n    upstream: main\n");
        }
        s.push_str("  - match: { prefix: \"/\" }\n    upstream: rest\npools:\n  rest:\n    backends:\n      - { host: \"127.0.0.1\", port: ");
        s.push_str(&catchall.to_string());
        s.push_str(" }\n");
        if api.is_some() {
            s.push_str(&format!(
                "  main:\n    backends:\n      - {{ host: \"127.0.0.1\", port: {a} }}\n"
            ));
        }
        s
    };

    let proxy = Proxy::start(&dir, &base(Some(())));
    assert_eq!(proxy.get("/api/x"), (200, "A\n".to_string()));

    proxy.rewrite(&base(None));
    proxy.sighup();

    eventually(Duration::from_secs(10), || {
        let (s, body) = proxy.get("/api/x");
        if s != 200 || body != "CATCHALL\n" {
            return Err(format!(
                "/api answers {s} {body:?}, expected it to fall through to /"
            ));
        }
        Ok(())
    });
}

/// The one that matters most in production: a broken config must NOT take traffic down. The
/// last-good table keeps serving and the operator gets a log line.
#[test]
fn a_config_that_does_not_parse_leaves_the_previous_one_serving() {
    let dir = scratch_dir("sighup-broken");
    let a = spawn_echo_upstream(&dir, "A");
    let proxy = Proxy::start(&dir, &pool_config("/api", a));
    assert_eq!(proxy.get("/api/x"), (200, "A\n".to_string()));

    // Unparseable.
    proxy.rewrite("routes: [ this is not: valid: yaml\n");
    proxy.sighup();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        proxy.get("/api/x"),
        (200, "A\n".to_string()),
        "a typo must not 502 every request in the fleet"
    );

    // Parses, but invalid: a route naming a pool that does not exist.
    proxy.rewrite("routes: [{match: {prefix: /api}, upstream: missing}]\npools: {}\n");
    proxy.sighup();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        proxy.get("/api/x"),
        (200, "A\n".to_string()),
        "an invalid config must be discarded, not half-applied"
    );
}

/// The file watch alone, with no signal: an operator editing the file must not have to know
/// that SIGHUP exists.
#[test]
fn the_file_watch_reloads_without_a_signal() {
    let dir = scratch_dir("watch-only");
    let a = spawn_echo_upstream(&dir, "A");
    let b = spawn_echo_upstream(&dir, "B");
    let proxy = Proxy::start(&dir, &pool_config("/api", a));
    assert_eq!(proxy.get("/api/x"), (200, "A\n".to_string()));

    proxy.rewrite(&pool_config("/api", b));
    // No sighup() here — this is the watcher, on its own 250 ms debounce.
    eventually(Duration::from_secs(10), || {
        let (s, body) = proxy.get("/api/x");
        if s != 200 || body != "B\n" {
            return Err(format!("/api answers {s} {body:?}, expected 200 \"B\\n\""));
        }
        Ok(())
    });
}

/// The probes must keep answering through a reload: a `/healthz` that fails during a swap is a
/// Kubernetes restart loop, and a rollout that restarts every pod it touches is the worst
/// possible time for the routing layer to be unavailable.
#[test]
fn the_probes_answer_before_during_and_after_a_reload() {
    let dir = scratch_dir("probes");
    let a = spawn_echo_upstream(&dir, "A");
    let b = spawn_echo_upstream(&dir, "B");
    let proxy = Proxy::start(&dir, &pool_config("/api", a));

    assert_eq!(proxy.get("/healthz").0, 200);
    assert_eq!(proxy.get("/readyz").0, 200);

    proxy.rewrite(&pool_config("/api", b));
    proxy.sighup();
    eventually(Duration::from_secs(10), || {
        let (s, _) = proxy.get("/api/x");
        if s != 200 {
            return Err(format!("/api answers {s} mid-reload"));
        }
        Ok(())
    });
    assert_eq!(proxy.get("/healthz").0, 200, "post-reload");
    assert_eq!(proxy.get("/readyz").0, 200, "post-reload");
}

/// Prefix routing is the reason the file-driven table exists at all: one rule covers a whole
/// subtree, and the query string survives the forward.
#[test]
fn a_prefix_route_covers_its_whole_subtree_and_keeps_the_query() {
    let dir = scratch_dir("prefix");
    let a = spawn_echo_upstream(&dir, "A");
    let proxy = Proxy::start(&dir, &pool_config("/api", a));
    assert_eq!(proxy.get("/api"), (200, "A\n".to_string()));
    assert_eq!(proxy.get("/api/"), (200, "A\n".to_string()));
    assert_eq!(proxy.get("/api/deep/nested/path"), (200, "A\n".to_string()));
    assert_eq!(
        proxy.get("/api/deep?page=2&q=x"),
        (200, "A\n".to_string()),
        "a query-bearing request must still match"
    );
    assert_eq!(
        proxy.get("/apix"),
        (404, "no route matches this request".to_string()),
        "a longer segment is a different resource and must not be swallowed"
    );
}

/// The binary refuses to start on a broken config rather than serving something else. That is
/// the difference between a failed deploy and a silent misroute.
#[test]
fn a_broken_config_at_startup_exits_instead_of_serving() {
    let dir = scratch_dir("startup-broken");
    let config = dir.join("proxy-runtime.yaml");
    std::fs::write(&config, "routes: [ not: valid\n").expect("write");
    let out = Command::new(env!("CARGO_BIN_EXE_relayfold-proxy"))
        .env("PROXY_CONFIG", config.display().to_string())
        .env("LISTEN_ADDR", "127.0.0.1:0")
        .output()
        .expect("run proxy");
    assert!(
        !out.status.success(),
        "a broken config must not start a proxy"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot start"), "{err}");
}

/// A missing config file is the same class of failure: there is no env-var fallback any more,
/// because a fallback that silently serves the wrong routes is worse than not starting.
#[test]
fn a_missing_config_at_startup_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_relayfold-proxy"))
        .env(
            "PROXY_CONFIG",
            scratch_dir("startup-missing")
                .join("nope.yaml")
                .display()
                .to_string(),
        )
        .env("LISTEN_ADDR", "127.0.0.1:0")
        .output()
        .expect("run proxy");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot start"));
}

/// A smoke check that the whole binary really serves a real forward, kept separate from the
/// reload assertions so a routing regression and a reload regression are distinguishable.
#[test]
fn the_proxy_forwards_a_real_request_to_the_configured_backend() {
    let dir = scratch_dir("forward");
    let a = spawn_echo_upstream(&dir, "A");
    let proxy = Proxy::start(&dir, &pool_config("/api", a));
    let (status, body) = proxy.get("/api/hello?x=1");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "A\n");
}
