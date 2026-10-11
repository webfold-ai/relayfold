//! Log compaction against real files: replay equivalence on randomized histories, recovery from
//! a kill at every point of the protocol, and refusal to start on a lost / damaged / mismatched
//! snapshot. (The mutant-rejection half lives in `tests/negative_controls.rs`.)

mod common;

use common::*;
use relayfold_control::auth::{RawAuth, SignedCall, Signer};
use relayfold_control::compaction::LossyFs;
use relayfold_control::dto::{ConfigPushRequest, RegisterRequest};
use relayfold_control::json::Json;
use relayfold_control::store::{self, LOG_FILE, SNAPSHOT_FILE};
use relayfold_control::{Clock, ControlConfig, ControlError, ControlPlane, ManualClock, Stages};
use std::path::Path;
use std::sync::Arc;

fn cfg() -> ControlConfig {
    let mut c = config();
    c.compact_after = None;
    c.nonce_capacity = 100_000;
    c
}

fn open(dir: &Path, clock: &ManualClock, cfg: ControlConfig) -> ControlPlane {
    ControlPlane::open(dir, cfg, Arc::new(clock.clone()), Stages::REAL).expect("open")
}

fn register(
    p: &mut ControlPlane,
    clock: &ManualClock,
    s: &Signer,
    id: &str,
    addr: &str,
) -> Result<(), ControlError> {
    let req = RegisterRequest {
        id: id.into(),
        addr: addr.into(),
        version: "1".into(),
    };
    let body = Json(req.clone()).encode();
    let auth: RawAuth = s
        .sign_now("POST", "/register", &body, clock.now_unix())
        .to_raw();
    let call = SignedCall {
        method: "POST",
        path: "/register",
        body: &body,
        auth: &auth,
    };
    p.register(&call, req).map(|_| ())
}

fn push(p: &mut ControlPlane, clock: &ManualClock, s: &Signer) -> Result<u64, ControlError> {
    let req = ConfigPushRequest {
        target: "*".into(),
        config: "c".into(),
    };
    let body = Json(req.clone()).encode();
    let auth = s
        .sign_now("POST", "/config/push", &body, clock.now_unix())
        .to_raw();
    let call = SignedCall {
        method: "POST",
        path: "/config/push",
        body: &body,
        auth: &auth,
    };
    p.push_config(&call, &req).map(|r| r.config_version)
}

/// Tiny deterministic generator: the histories are reproducible from the seed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// One random step against a plane; returns a tag of the outcome so two planes can be compared.
fn step(
    rng: &mut Lcg,
    p: &mut ControlPlane,
    clock: &ManualClock,
    who: &[Signer],
    admin: &Signer,
    outsider: &Signer,
) -> String {
    let outcome = |r: Result<(), ControlError>| match r {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("err:{e}"),
    };
    match rng.next(10) {
        0..=5 => {
            let s = &who[rng.next(who.len() as u64) as usize];
            let id = format!("i{}", rng.next(6));
            let addr = format!("http://10.0.0.{}:{}", rng.next(250), rng.next(60000));
            outcome(register(p, clock, s, &id, &addr))
        }
        6 | 7 => match push(p, clock, admin) {
            Ok(v) => format!("push:{v}"),
            Err(e) => format!("err:{e}"),
        },
        _ => outcome(register(p, clock, outsider, "x", "http://x")),
    }
}

/// One random history, run on two planes (one compacts at random points with `stages`, one never
/// does), then both restarted from disk. `Ok(n)`: they agree, `n` compactions ran. `Err`: the
/// first disagreement — the verdict of this checker on `stages`.
fn run_seed(seed: u64, stages: Stages) -> Result<usize, String> {
    let who = [
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_B),
        Signer::from_dialkey(DK_C),
    ];
    let (admin, outsider) = (
        Signer::from_dialkey(DK_ADMIN),
        Signer::from_dialkey(DK_OUTSIDER),
    );
    let mut compactions = 0;
    let (da, db) = (tempdir("eq-compacted"), tempdir("eq-plain"));
    let clock = ManualClock::new(START);
    let mut a = ControlPlane::open(da.path(), cfg(), Arc::new(clock.clone()), stages)
        .map_err(|e| format!("open: {e}"))?;
    let mut b = open(db.path(), &clock, cfg());
    let (mut ra, mut rb) = (Lcg(seed), Lcg(seed));
    let mut when = Lcg(seed ^ 0xabcdef);
    for _ in 0..120 {
        let oa = step(&mut ra, &mut a, &clock, &who, &admin, &outsider);
        let ob = step(&mut rb, &mut b, &clock, &who, &admin, &outsider);
        if oa != ob {
            return Err(format!(
                "seed {seed}: the twins diverged before any restart"
            ));
        }
        if when.next(15) == 0 {
            a.compact().map_err(|e| format!("compact: {e}"))?;
            compactions += 1;
        }
    }
    // Restart both from disk: snapshot + tail on one side, the full log on the other.
    drop((a, b));
    let a = ControlPlane::open(da.path(), cfg(), Arc::new(clock.clone()), Stages::REAL)
        .map_err(|e| format!("seed {seed}: the compacted store cannot be opened: {e}"))?;
    let b = open(db.path(), &clock, cfg());
    if a.state() != b.state() {
        return Err(format!(
            "seed {seed}: replay(snapshot + tail) != replay(full log)"
        ));
    }
    // And the replay the store itself does agrees with the live state it was rebuilt into.
    drop(a);
    let (replayed, _sink) = store::open(da.path()).map_err(|e| format!("{e}"))?;
    if replayed.canonical() != b.state() {
        return Err(format!("seed {seed}: store::open disagrees with the plane"));
    }
    Ok(compactions)
}

/// Run seeds 1..=8 side by side (the work is fsync-bound); every seed's verdict, in order.
fn run_seeds(stages: Stages) -> Vec<Result<usize, String>> {
    std::thread::scope(|s| {
        let handles: Vec<_> = (1..=8u64)
            .map(|seed| s.spawn(move || run_seed(seed, stages)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a seed's thread panicked"))
            .collect()
    })
}

#[test]
fn replay_of_snapshot_plus_tail_equals_replay_of_the_full_log_on_random_histories() {
    let verdicts = run_seeds(Stages::REAL);
    let mut compactions = 0;
    for v in verdicts {
        compactions += v.expect("the real implementation must pass");
    }
    assert!(compactions > 20, "the histories must really compact");
}

#[test]
fn the_random_history_checker_rejects_lossy_snapshots_made_by_the_real_compaction() {
    // The same checker, the real plane and log, a snapshot that forgets part of the state.
    let lossy = [
        (
            "drops every instance",
            Stages {
                snapshot_shape: |mut s| {
                    s.instances.clear();
                    s
                },
                ..Stages::REAL
            },
        ),
        (
            "drops the audit trail",
            Stages {
                snapshot_shape: |mut s| {
                    s.rejections.clear();
                    s
                },
                ..Stages::REAL
            },
        ),
        (
            "forgets the bound keys",
            Stages {
                snapshot_shape: |mut s| {
                    for b in s.instances.values_mut() {
                        b.key = [0; 32];
                    }
                    s
                },
                ..Stages::REAL
            },
        ),
    ];
    for (name, stages) in lossy {
        let verdicts = run_seeds(stages);
        assert!(
            verdicts.iter().any(|v| v.is_err()),
            "MUTANT SURVIVED the random-history checker on 8 seeds: snapshot {name}"
        );
    }
    // And a do-nothing struct update is still accepted: the checker is not rejecting the shape.
    let same = Stages {
        snapshot_shape: |s| s,
        ..Stages::REAL
    };
    assert!(run_seeds(same).iter().all(|v| v.is_ok()));
}

#[test]
fn a_kill_between_the_snapshot_and_the_truncation_recovers_and_finishes_the_truncation() {
    let dir = tempdir("kill-mid");
    let clock = ManualClock::new(START);
    let (a, outsider) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_OUTSIDER),
    );
    // Real plan: 0 WriteSnapshot, 1 FsyncSnapshot, 2 RenameSnapshot, 3 FsyncDir | 4 WriteLog ...
    // Kill after 4 operations = the snapshot is durable, the log is untouched.
    let lossy = LossyFs::new(Some(4));
    let mut p = ControlPlane::open_with_fs(
        dir.path(),
        cfg(),
        Arc::new(clock.clone()),
        Stages::REAL,
        Box::new(lossy.clone()),
    )
    .unwrap();
    for i in 0..5 {
        register(&mut p, &clock, &a, &format!("i{i}"), "http://h").unwrap();
    }
    for _ in 0..2 {
        assert!(matches!(
            register(&mut p, &clock, &outsider, "evil", "http://e"),
            Err(ControlError::NotEnrolled)
        ));
    }
    let expected = p.state();
    assert_eq!(expected.2.len(), 2);
    assert!(
        p.compact().is_err(),
        "the injected kill aborts the compaction"
    );
    assert_eq!(lossy.ops_done(), 4);
    // The poisoned log refuses to take a record the snapshot would not contain.
    assert!(matches!(
        register(&mut p, &clock, &a, "late", "http://h"),
        Err(ControlError::Storage(_))
    ));
    drop(p);

    assert!(
        dir.path().join(SNAPSHOT_FILE).exists(),
        "snapshot is durable"
    );
    let log_before = std::fs::metadata(dir.path().join(LOG_FILE)).unwrap().len();
    assert!(log_before > 100, "the log was not truncated yet");

    let mut p = open(dir.path(), &clock, cfg());
    assert_eq!(p.state(), expected, "no record lost");
    assert_eq!(
        p.state().2.len(),
        2,
        "no record applied twice (the audit ring would double)"
    );
    assert_eq!(p.tail_records(), 0);
    let log_after = std::fs::metadata(dir.path().join(LOG_FILE)).unwrap().len();
    assert!(
        log_after < log_before,
        "recovery finished the truncation: {log_after} < {log_before}"
    );
    // The recovered store takes writes into the new epoch, and they survive another restart.
    register(&mut p, &clock, &a, "after", "http://h").unwrap();
    drop(p);
    let p = open(dir.path(), &clock, cfg());
    assert!(p.bound_key("after").is_some());
    assert_eq!(p.state().0.len(), 6);
    assert_eq!(p.state().2.len(), 2);
}

fn compacted_dir() -> (TempDir, ManualClock) {
    let dir = tempdir("refuse");
    let clock = ManualClock::new(START);
    let a = Signer::from_dialkey(DK_A);
    let mut p = open(dir.path(), &clock, cfg());
    register(&mut p, &clock, &a, "i1", "http://h").unwrap();
    p.compact().unwrap();
    (dir, clock)
}

fn open_err(dir: &Path, clock: &ManualClock) -> String {
    match ControlPlane::open(dir, cfg(), Arc::new(clock.clone()), Stages::REAL) {
        Ok(_) => panic!("the store must refuse to open"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_lost_snapshot_is_refused_not_mistaken_for_an_empty_fleet() {
    let (dir, clock) = compacted_dir();
    std::fs::remove_file(dir.path().join(SNAPSHOT_FILE)).unwrap();
    let e = open_err(dir.path(), &clock);
    assert!(e.contains("no snapshot"), "{e}");
}

#[test]
fn a_damaged_snapshot_is_refused() {
    let (dir, clock) = compacted_dir();
    let path = dir.path().join(SNAPSHOT_FILE);
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x40;
    std::fs::write(&path, &bytes).unwrap();
    let e = open_err(dir.path(), &clock);
    assert!(e.contains("snapshot"), "{e}");
    // An empty file (the zero-length-after-rename failure) is refused too.
    std::fs::write(&path, b"").unwrap();
    let e = open_err(dir.path(), &clock);
    assert!(e.contains("snapshot"), "{e}");
}

#[test]
fn a_snapshot_from_the_wrong_epoch_is_refused() {
    let dir = tempdir("epoch");
    let clock = ManualClock::new(START);
    let a = Signer::from_dialkey(DK_A);
    let mut p = open(dir.path(), &clock, cfg());
    register(&mut p, &clock, &a, "i1", "http://h").unwrap();
    p.compact().unwrap();
    let old = std::fs::read(dir.path().join(SNAPSHOT_FILE)).unwrap();
    register(&mut p, &clock, &a, "i2", "http://h").unwrap();
    p.compact().unwrap();
    drop(p);
    // Put the first snapshot back: it covers epoch 0, the log is epoch 2.
    std::fs::write(dir.path().join(SNAPSHOT_FILE), old).unwrap();
    let e = open_err(dir.path(), &clock);
    assert!(e.contains("covers epoch"), "{e}");
}

#[test]
fn automatic_compaction_bounds_the_log_and_loses_nothing() {
    let dir = tempdir("auto");
    let clock = ManualClock::new(START);
    let who = [Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B)];
    let mut c = cfg();
    c.compact_after = Some(10);
    let mut p = open(dir.path(), &clock, c.clone());
    let mut max_log = 0;
    let mut max_tail = 0;
    for i in 0..200 {
        // Eight ids, constantly re-addressed: 200 records, 8 live instances.
        let id = format!("i{}", i % 8);
        register(&mut p, &clock, &who[0], &id, &format!("http://h/{i}")).unwrap();
        max_tail = max_tail.max(p.tail_records());
        max_log = max_log.max(std::fs::metadata(dir.path().join(LOG_FILE)).unwrap().len());
    }
    assert!(
        max_tail <= 10,
        "the tail never exceeds the threshold: {max_tail}"
    );
    let one_record = 8 + 1 + 4 * 4 + 2 + 20 + 1 + 32; // a rough upper bound for one frame
    assert!(
        max_log < 12 * (one_record as u64) + 64,
        "the log stays within ~threshold records, got {max_log} bytes"
    );
    let expected = p.state();
    assert_eq!(expected.0.len(), 8);
    drop(p);
    let p = open(dir.path(), &clock, c);
    assert_eq!(p.state(), expected);
}

#[test]
fn a_compaction_that_fails_before_anything_is_visible_does_not_stop_the_plane() {
    // Kill after 1 operation: the snapshot tmp file was written (think ENOSPC halfway), nothing
    // was renamed. The log is untouched and must stay writable; the failed attempt must not make
    // later writes 500 until a restart.
    let dir = tempdir("fail-early");
    let clock = ManualClock::new(START);
    let a = Signer::from_dialkey(DK_A);
    let mut p = ControlPlane::open_with_fs(
        dir.path(),
        cfg(),
        Arc::new(clock.clone()),
        Stages::REAL,
        Box::new(LossyFs::new(Some(1))),
    )
    .unwrap();
    register(&mut p, &clock, &a, "i1", "http://h").unwrap();
    assert!(p.compact().is_err());
    register(&mut p, &clock, &a, "i2", "http://h").expect("still writable");
    let expected = p.state();
    drop(p);
    assert!(
        !dir.path().join(SNAPSHOT_FILE).exists(),
        "the failed attempt left no visible snapshot"
    );
    let p = open(dir.path(), &clock, cfg());
    assert_eq!(p.state(), expected, "both records survived the restart");
}

#[test]
fn a_crash_inside_recovery_itself_is_recoverable() {
    // Double fault: the first kill leaves "snapshot durable, log not yet replaced" (4 operations
    // of the real plan). Recovery then has to run the FINISH plan (write log, fsync, rename, dir
    // fsync); kill *that* after every prefix, with and without power loss, then recover cleanly.
    let seed = tempdir("double-fault-seed");
    let clock = ManualClock::new(START);
    let (a, outsider) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_OUTSIDER),
    );
    let mut p = ControlPlane::open_with_fs(
        seed.path(),
        cfg(),
        Arc::new(clock.clone()),
        Stages::REAL,
        Box::new(LossyFs::new(Some(4))),
    )
    .unwrap();
    for i in 0..4 {
        register(&mut p, &clock, &a, &format!("i{i}"), "http://h").unwrap();
    }
    let _ = register(&mut p, &clock, &outsider, "evil", "http://e");
    let expected = p.state();
    assert!(p.compact().is_err());
    drop(p);

    for power_loss in [false, true] {
        for k in 0..=4usize {
            let dir = tempdir("double-fault");
            for f in [LOG_FILE, SNAPSHOT_FILE] {
                std::fs::copy(seed.path().join(f), dir.path().join(f)).unwrap();
            }
            let lossy = LossyFs::new(Some(k));
            let first = store::open_with_fs(dir.path(), Box::new(lossy.clone()));
            if k < 4 {
                assert!(first.is_err(), "recovery killed after {k} of 4 operations");
            }
            drop(first);
            if power_loss {
                lossy.power_loss().unwrap();
            }
            let p = open(dir.path(), &clock, cfg());
            assert_eq!(
                p.state(),
                expected,
                "recovery killed after {k} operations (power loss: {power_loss})"
            );
            assert_eq!(p.tail_records(), 0);
        }
    }
}
