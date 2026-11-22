//! `relayfold-bench <path> <duration_secs> <connections> <name=host:port> [name=host:port ...]`
//!
//! Runs the SAME GET workload against every named subject in sequence (not concurrently — two
//! subjects competing for the same CPU cores would make the numbers meaningless as a comparison)
//! and prints one table. Every row is a real, measured run; a subject that couldn't be reached is
//! printed as `UNREACHABLE: <reason>`, never a silently-zeroed row sitting next to real numbers.

use relayfold_bench::{run, Workload};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: relayfold-bench <path> <duration_secs> <connections> <name=host:port> [name=host:port ...]");
        std::process::exit(2);
    }
    let path = &args[1];
    let duration: u64 = args[2]
        .parse()
        .expect("duration_secs must be a real integer");
    let connections: usize = args[3].parse().expect("connections must be a real integer");

    let subjects: Vec<(String, String, u16)> = args[4..]
        .iter()
        .map(|spec| {
            let (name, addr) = spec
                .split_once('=')
                .unwrap_or_else(|| panic!("expected name=host:port, got {spec:?}"));
            let (host, port) = addr
                .split_once(':')
                .unwrap_or_else(|| panic!("expected host:port, got {addr:?}"));
            (
                name.to_string(),
                host.to_string(),
                port.parse()
                    .unwrap_or_else(|_| panic!("bad port in {spec:?}")),
            )
        })
        .collect();

    println!("workload: GET {path}, {duration}s, {connections} connections per subject\n");
    println!(
        "{:<20} {:>12} {:>10} {:>10} {:>10} {:>10}",
        "subject", "req/s", "errors", "p50 ms", "p95 ms", "p99 ms"
    );

    for (name, host, port) in subjects {
        let workload = Workload::get(&host, port, path)
            .with_duration(Duration::from_secs(duration))
            .with_connections(connections);
        match run(&name, &workload) {
            Ok(report) => {
                println!(
                    "{:<20} {:>12.1} {:>10} {:>10.3} {:>10.3} {:>10.3}",
                    report.name,
                    report.throughput_rps(),
                    report.errors,
                    report.p50_ms().unwrap_or(0.0),
                    report.p95_ms().unwrap_or(0.0),
                    report.p99_ms().unwrap_or(0.0),
                );
            }
            Err(e) => println!("{:<20} UNREACHABLE: {}", name, e.message),
        }
    }
}
