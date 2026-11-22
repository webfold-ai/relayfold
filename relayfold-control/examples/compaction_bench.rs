//! Compaction benchmark on a SYNTHETIC log. `cargo run --release --example compaction_bench`.
//!
//! For each N: write a log of N synthetic events (60% re-registers over 1000 ids, 30% config
//! pushes, 10% enrollment refusals; deterministic generator) plus a tail of T more, in two
//! directories with identical bytes. One directory is never compacted (the baseline); the other is
//! compacted after the first N events and then receives the same T-event tail. Reported, all on
//! that same data:
//!
//! * quality — `replay(snapshot + tail) == replay(full log)` (and both equal an in-memory fold);
//! * speed   — compaction wall time; `store::open` wall time (median of 5) with and without it;
//! * baseline — the never-compacted directory's bytes and open time.
//!
//! The synthetic log is written with `FsyncPolicy::Manual` (one fsync at the end) so that building
//! 10^6 events does not take hours; the bytes are exactly what the control plane's `EveryRecord`
//! path writes. Compaction itself runs the real [`ControlPlane::compact`] with the real fsyncs.

use iotadb_log::{FileWriter, FsyncPolicy, Log};
use relayfold_control::auth::Key;
use relayfold_control::store::{self, Record, Rejection, Replayed, LOG_FILE, SNAPSHOT_FILE};
use relayfold_control::{ControlConfig, ControlPlane, ManualClock, Stages};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const TAIL: usize = 1_000;
const IDS: u64 = 1_000;

struct Gen(u64);

impl Gen {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

fn key_of(id: u64) -> Key {
    let mut k = [0u8; 32];
    k[..8].copy_from_slice(&id.to_be_bytes());
    k
}

/// The `i`-th synthetic event of the stream seeded by `seed`.
fn events(seed: u64, count: usize, first_index: usize, first_version: u64) -> Vec<Record> {
    let mut g = Gen(seed.wrapping_add(first_index as u64));
    let mut version = first_version;
    (0..count)
        .map(|j| {
            let i = first_index + j;
            match g.next(10) {
                0..=5 => {
                    let id = g.next(IDS);
                    Record::Registered {
                        id: format!("proxy-{id}"),
                        addr: format!(
                            "http://10.{}.{}.{}:{}",
                            id % 250,
                            g.next(250),
                            g.next(250),
                            8000 + g.next(1000)
                        ),
                        version: format!("0.1.{}", g.next(50)),
                        key: key_of(id),
                    }
                }
                6..=8 => {
                    version += 1;
                    Record::ConfigPushed { version }
                }
                _ => Record::EnrollmentRejected(Rejection {
                    key: key_of(1_000_000 + g.next(5000)),
                    id: format!("intruder-{i}"),
                    at: 1_700_000_000 + i as i64,
                }),
            }
        })
        .collect()
}

fn max_version(recs: &[Record]) -> u64 {
    recs.iter()
        .filter_map(|r| match r {
            Record::ConfigPushed { version } => Some(*version),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn write_log(dir: &Path, recs: &[Record]) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(LOG_FILE);
    let w = FileWriter::open_append(&path).unwrap();
    let (mut log, _) = Log::open(w, path, FsyncPolicy::Manual).unwrap();
    for r in recs {
        log.append(&r.encode()).unwrap();
    }
    log.sync().unwrap();
}

fn append_to_dir(dir: &Path, recs: &[Record]) {
    write_log(dir, recs);
}

fn median_open_ms(dir: &Path) -> f64 {
    let mut t: Vec<f64> = (0..5)
        .map(|_| {
            let start = Instant::now();
            let (replayed, sink) = store::open(dir).unwrap();
            let d = start.elapsed().as_secs_f64() * 1000.0;
            drop((replayed, sink));
            d
        })
        .collect();
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    t[2]
}

fn size(dir: &Path, file: &str) -> u64 {
    std::fs::metadata(dir.join(file))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn main() {
    let root: PathBuf = std::env::var("BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target/compaction-bench"));
    let _ = std::fs::remove_dir_all(&root);
    println!("SYNTHETIC data: {TAIL}-event tail after each N; 1000 ids; 60% register / 30% push / 10% refusal");
    println!();
    println!("| N events | log bytes (baseline, no compaction) | open ms (baseline) | compact ms | log bytes after | snapshot bytes | open ms (snapshot) | open speedup | replay(snap+tail) == replay(full) |");
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|:---:|");
    for n in [1_000usize, 10_000, 100_000, 1_000_000] {
        let head = events(7, n, 0, 0);
        let tail = events(7, TAIL, n, max_version(&head));
        let (full, compacted) = (
            root.join(format!("{n}-full")),
            root.join(format!("{n}-compacted")),
        );

        // Baseline: head + tail, never compacted.
        write_log(&full, &head);
        append_to_dir(&full, &tail);
        let full_bytes = size(&full, LOG_FILE);
        let full_open = median_open_ms(&full);

        // Compacted: head, compact (the real ControlPlane::compact), then the same tail.
        write_log(&compacted, &head);
        let (replayed, sink) = store::open(&compacted).unwrap();
        let mut cfg = ControlConfig::new("bench-control", vec![]);
        cfg.compact_after = None;
        let mut plane = ControlPlane::from_parts(
            cfg,
            Arc::new(ManualClock::new(1_700_000_000)),
            Stages::REAL,
            replayed,
            Box::new(sink),
        );
        let t = Instant::now();
        let report = plane.compact().unwrap();
        let compact_ms = t.elapsed().as_secs_f64() * 1000.0;
        drop(plane);
        append_to_dir(&compacted, &tail);
        let snap_bytes = size(&compacted, SNAPSHOT_FILE);
        let log_after = size(&compacted, LOG_FILE) - size_of_tail(&tail);
        let snap_open = median_open_ms(&compacted);

        // Quality: snapshot + tail == full log == an in-memory fold of every event.
        let (a, _) = store::open(&compacted).unwrap();
        let (b, _) = store::open(&full).unwrap();
        let mut model = Replayed::default();
        for r in head.iter().chain(tail.iter()) {
            model.apply(r.clone());
        }
        let equal = a.canonical() == b.canonical() && a.canonical() == model.canonical();
        assert!(report.records_folded as usize == n);

        println!(
            "| {n} | {full_bytes} | {full_open:.1} | {compact_ms:.1} | {log_after} | {snap_bytes} | {snap_open:.1} | {:.1}x | {} |",
            full_open / snap_open,
            if equal { "PASS" } else { "FAIL" }
        );
        assert!(equal, "replay equivalence failed at N={n}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Bytes the tail adds to the log (8-byte frame header + payload each).
fn size_of_tail(tail: &[Record]) -> u64 {
    tail.iter().map(|r| 8 + r.encode().len() as u64).sum()
}
