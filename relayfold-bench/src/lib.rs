//! A real load-generation/latency harness — one workload (method/path/duration/concurrency)
//! run identically against N named HTTP subjects, so every reported number has a same-workload
//! baseline beside it rather than standing alone.
//!
//! Zero-dep on purpose: `client::request_plain_on` (plain HTTP/1.1, keep-alive, std only)
//! is the wire client — every subject here is plain loopback HTTP, so TLS is pure overhead this
//! harness doesn't need. `std::thread`, not tokio: `connections` OS threads, each holding one
//! persistent TCP connection and firing requests back-to-back for `duration` — the same shape
//! `wrk`/`hey` use, not a fresh-connection-per-request generator (which would measure connection
//! setup, not the server's steady-state request handling).
//!
//! **Honest scope**: no HTTP/2, no TLS, no request-body payloads (GET-shaped workload only).
//! These are real limits of what got built, not gaps papered over — see README for what a
//! from-scratch competitor comparison genuinely needs beyond this.

pub mod client;
use client::request_plain_on;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Workload {
    pub host: String,
    pub port: u16,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub duration: Duration,
    pub connections: usize,
}

impl Workload {
    #[must_use]
    pub fn get(host: impl Into<String>, port: u16, path: impl Into<String>) -> Self {
        Workload {
            host: host.into(),
            port,
            method: "GET".into(),
            path: path.into(),
            headers: Vec::new(),
            duration: Duration::from_secs(10),
            connections: 8,
        }
    }

    #[must_use]
    pub fn with_duration(mut self, d: Duration) -> Self {
        self.duration = d;
        self
    }

    #[must_use]
    pub fn with_connections(mut self, n: usize) -> Self {
        self.connections = n.max(1);
        self
    }
}

/// One subject's real, measured result. Every field here came from an actual completed run — a
/// subject that couldn't be reached at all is a `RunError`, never a zeroed-out `Report`.
#[derive(Clone, Debug)]
pub struct Report {
    pub name: String,
    pub total_requests: u64,
    pub errors: u64,
    pub wall_time: Duration,
    /// Sorted, ascending — percentiles are read straight off this, no interpolation guesswork
    /// beyond a plain index (`p * len`), disclosed in `percentile`'s own doc.
    latencies_us: Vec<u64>,
}

impl Report {
    #[must_use]
    pub fn throughput_rps(&self) -> f64 {
        if self.wall_time.as_secs_f64() == 0.0 {
            return 0.0;
        }
        self.total_requests as f64 / self.wall_time.as_secs_f64()
    }

    /// `p` in `[0.0, 1.0]`. Nearest-rank on the sorted sample — the same method `wrk`'s own
    /// histogram uses; no interpolation between the two nearest samples.
    #[must_use]
    pub fn percentile_us(&self, p: f64) -> Option<u64> {
        if self.latencies_us.is_empty() {
            return None;
        }
        let idx = ((self.latencies_us.len() as f64 - 1.0) * p.clamp(0.0, 1.0)).round() as usize;
        self.latencies_us.get(idx).copied()
    }

    #[must_use]
    pub fn p50_ms(&self) -> Option<f64> {
        self.percentile_us(0.50).map(|us| us as f64 / 1000.0)
    }

    #[must_use]
    pub fn p95_ms(&self) -> Option<f64> {
        self.percentile_us(0.95).map(|us| us as f64 / 1000.0)
    }

    #[must_use]
    pub fn p99_ms(&self) -> Option<f64> {
        self.percentile_us(0.99).map(|us| us as f64 / 1000.0)
    }
}

#[derive(Debug)]
pub struct RunError {
    pub name: String,
    pub message: String,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}

/// Run `workload` against one subject for real, over `workload.connections` persistent
/// connections, for `workload.duration`. Returns `Err` if the subject can't even be connected to
/// at the start — never a `Report` with zeroed fields standing in for "it didn't work."
pub fn run(name: impl Into<String>, workload: &Workload) -> Result<Report, RunError> {
    let name = name.into();
    // Fail fast, once, before spawning any worker thread — a subject that's down should report
    // as down, not as "0 req/s" indistinguishable from a real (if terrible) result.
    TcpStream::connect((workload.host.as_str(), workload.port)).map_err(|e| RunError {
        name: name.clone(),
        message: format!("initial connect failed: {e}"),
    })?;

    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let latencies: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    let start = Instant::now();
    let handles: Vec<_> = (0..workload.connections)
        .map(|_| {
            let workload = workload.clone();
            let stop = Arc::clone(&stop);
            let total = Arc::clone(&total);
            let errors = Arc::clone(&errors);
            let latencies = Arc::clone(&latencies);
            std::thread::spawn(move || {
                worker_loop(&workload, &stop, &total, &errors, &latencies);
            })
        })
        .collect();

    std::thread::sleep(workload.duration);
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    let wall_time = start.elapsed();

    let mut latencies_us = Arc::try_unwrap(latencies)
        .expect("all worker threads joined")
        .into_inner()
        .unwrap();
    latencies_us.sort_unstable();

    Ok(Report {
        name,
        total_requests: total.load(Ordering::Relaxed),
        errors: errors.load(Ordering::Relaxed),
        wall_time,
        latencies_us,
    })
}

fn worker_loop(
    workload: &Workload,
    stop: &AtomicBool,
    total: &AtomicU64,
    errors: &AtomicU64,
    latencies: &Mutex<Vec<u64>>,
) {
    // One persistent connection per worker — reconnect on error rather than dying, so one
    // dropped keep-alive connection doesn't silently end that worker's whole contribution.
    let Ok(mut conn) = TcpStream::connect((workload.host.as_str(), workload.port)) else {
        errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let mut local_latencies = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        match request_plain_on(
            &mut conn,
            &workload.host,
            workload.port,
            &workload.method,
            &workload.path,
            &workload.headers,
            &[],
        ) {
            Ok(resp) if resp.status < 500 => {
                local_latencies.push(t0.elapsed().as_micros() as u64);
                total.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                errors.fetch_add(1, Ordering::Relaxed);
                // The connection may be dead (peer closed keep-alive) — reconnect once rather
                // than spinning a hot error loop against a socket that will never answer again.
                if let Ok(fresh) = TcpStream::connect((workload.host.as_str(), workload.port)) {
                    conn = fresh;
                } else {
                    break;
                }
            }
        }
    }

    if let Ok(mut guard) = latencies.lock() {
        guard.extend(local_latencies);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A minimal real HTTP/1.1 server — enough to prove `run` measures a REAL round trip, not a
    /// mocked one. Keep-alive: reads one request, writes one response, loops on the same socket.
    fn spawn_echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || loop {
                    let mut buf = [0u8; 4096];
                    let Ok(n) = stream.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    let body = b"ok";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(resp.as_bytes()).is_err() || stream.write_all(body).is_err()
                    {
                        break;
                    }
                });
            }
        });
        port
    }

    #[test]
    fn a_real_run_against_a_real_server_produces_a_nonzero_report() {
        let port = spawn_echo_server();
        std::thread::sleep(Duration::from_millis(50));
        let workload = Workload::get("127.0.0.1", port, "/")
            .with_duration(Duration::from_millis(300))
            .with_connections(2);
        let report = run("echo", &workload).expect("a real listening server must not error");
        assert!(
            report.total_requests > 0,
            "a 300ms run against a real local server must complete at least one request"
        );
        assert_eq!(report.errors, 0);
        assert!(report.p50_ms().unwrap() >= 0.0);
        assert!(report.throughput_rps() > 0.0);
    }

    #[test]
    fn an_unreachable_subject_is_a_real_error_not_a_zeroed_report() {
        // Port 1 is a real, well-known unassigned/unused low port — connecting to it on
        // loopback with nothing listening must fail fast and deterministically.
        let workload = Workload::get("127.0.0.1", 1, "/").with_duration(Duration::from_millis(50));
        let result = run("nothing-listening", &workload);
        assert!(
            result.is_err(),
            "an unreachable subject must be Err, never a Report claiming 0 req/s as if it ran"
        );
    }

    #[test]
    fn percentiles_are_monotonically_non_decreasing() {
        let port = spawn_echo_server();
        std::thread::sleep(Duration::from_millis(50));
        let workload = Workload::get("127.0.0.1", port, "/")
            .with_duration(Duration::from_millis(300))
            .with_connections(4);
        let report = run("echo", &workload).unwrap();
        assert!(report.p50_ms().unwrap() <= report.p95_ms().unwrap());
        assert!(report.p95_ms().unwrap() <= report.p99_ms().unwrap());
    }
}
