//! Restart and torn-tail behaviour of the append log, against real files.

mod common;

use common::*;
use relayfold_control::auth::{public_key_of, RawAuth, SignedCall, Signer};
use relayfold_control::dto::{ConfigPushRequest, HeartbeatRequest, RegisterRequest};
use relayfold_control::json::Json;
use relayfold_control::store::{self, LOG_FILE};
use relayfold_control::{ControlConfig, ControlError, ControlPlane, ManualClock, Stages};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

fn open(dir: &Path, clock: &ManualClock) -> ControlPlane {
    ControlPlane::open(dir, config(), Arc::new(clock.clone()), Stages::REAL).expect("open")
}

fn register(
    p: &mut ControlPlane,
    clock: &ManualClock,
    s: &Signer,
    id: &str,
) -> Result<(), ControlError> {
    use relayfold_control::Clock;
    let req = RegisterRequest {
        id: id.into(),
        addr: format!("http://127.0.0.1/{id}"),
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

fn heartbeat(
    p: &mut ControlPlane,
    clock: &ManualClock,
    s: &Signer,
    id: &str,
) -> Result<(), ControlError> {
    use relayfold_control::Clock;
    let req = HeartbeatRequest {
        id: id.into(),
        healthy: true,
        config_version: "v".into(),
    };
    let body = Json(req.clone()).encode();
    let auth = s
        .sign_now("POST", "/heartbeat", &body, clock.now_unix())
        .to_raw();
    let call = SignedCall {
        method: "POST",
        path: "/heartbeat",
        body: &body,
        auth: &auth,
    };
    p.heartbeat(&call, req).map(|_| ())
}

fn push(
    p: &mut ControlPlane,
    clock: &ManualClock,
    s: &Signer,
    target: &str,
) -> Result<u64, ControlError> {
    use relayfold_control::Clock;
    let req = ConfigPushRequest {
        target: target.into(),
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
    p.push_config(&call, &req).map(|pp| pp.config_version)
}

#[test]
fn identities_bound_keys_and_config_version_survive_a_restart() {
    let dir = tempdir("durable");
    let clock = ManualClock::new(START);
    let (a, b, admin) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_B),
        Signer::from_dialkey(DK_ADMIN),
    );
    {
        let mut p = open(dir.path(), &clock);
        register(&mut p, &clock, &a, "i1").unwrap();
        register(&mut p, &clock, &b, "i2").unwrap();
        assert_eq!(push(&mut p, &clock, &admin, "i1").unwrap(), 1);
        assert_eq!(push(&mut p, &clock, &admin, "*").unwrap(), 2);
    } // process "exits": every handle dropped

    let mut p = open(dir.path(), &clock);
    assert_eq!(
        p.recovery().bytes_dropped,
        0,
        "a clean shutdown leaves nothing to truncate"
    );
    assert_eq!(p.bound_key("i1"), Some(a.public_key()));
    assert_eq!(p.bound_key("i2"), Some(b.public_key()));
    assert_eq!(p.config_version(), 2);
    assert_eq!(p.control_public_key(), public_key_of(DK_CONTROL));

    // The binding is enforced after the restart, not just remembered.
    assert!(matches!(
        heartbeat(&mut p, &clock, &b, "i1"),
        Err(ControlError::Forbidden(_))
    ));
    assert!(matches!(
        register(&mut p, &clock, &b, "i1"),
        Err(ControlError::Forbidden(_))
    ));
    assert!(heartbeat(&mut p, &clock, &a, "i1").is_ok());
    // And the version counter continues, it does not restart at 1.
    assert_eq!(push(&mut p, &clock, &admin, "i1").unwrap(), 3);
}

#[test]
fn heartbeats_are_not_logged_and_liveness_is_unknown_after_restart() {
    let dir = tempdir("liveness");
    let clock = ManualClock::new(START);
    let a = Signer::from_dialkey(DK_A);
    {
        let mut p = open(dir.path(), &clock);
        register(&mut p, &clock, &a, "i1").unwrap();
        let before = std::fs::metadata(dir.path().join(LOG_FILE)).unwrap().len();
        for _ in 0..20 {
            heartbeat(&mut p, &clock, &a, "i1").unwrap();
        }
        let after = std::fs::metadata(dir.path().join(LOG_FILE)).unwrap().len();
        assert_eq!(before, after, "20 heartbeats must not grow the log");
        // Re-registering unchanged is also not a write.
        register(&mut p, &clock, &a, "i1").unwrap();
        assert_eq!(
            after,
            std::fs::metadata(dir.path().join(LOG_FILE)).unwrap().len()
        );
    }
    let mut p = open(dir.path(), &clock);
    assert!(
        p.list().iter().all(|i| !i.healthy),
        "nothing is healthy until it is heard from"
    );
    heartbeat(&mut p, &clock, &a, "i1").unwrap();
    assert!(p.list()[0].healthy);
}

#[test]
fn a_torn_tail_is_truncated_earlier_records_survive_and_new_writes_stick() {
    let dir = tempdir("torn");
    let clock = ManualClock::new(START);
    let (a, b, c) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_B),
        Signer::from_dialkey("http-test-instance-c"),
    );
    {
        let mut p = open(dir.path(), &clock);
        register(&mut p, &clock, &a, "i1").unwrap();
        register(&mut p, &clock, &b, "i2").unwrap();
    }
    let log = dir.path().join(LOG_FILE);
    let good_len = std::fs::metadata(&log).unwrap().len();

    // A crash mid-append: a frame header promising 200 payload bytes, then only 11 of them.
    let mut torn = Vec::new();
    torn.extend_from_slice(&200u32.to_be_bytes());
    torn.extend_from_slice(&0xdead_beefu32.to_be_bytes());
    torn.extend_from_slice(&[7u8; 11]);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(&torn)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&log).unwrap().len(),
        good_len + torn.len() as u64
    );

    let mut p = open(dir.path(), &clock);
    assert_eq!(
        p.recovery().bytes_dropped,
        torn.len() as u64,
        "exactly the torn bytes are dropped"
    );
    assert_eq!(
        std::fs::metadata(&log).unwrap().len(),
        good_len,
        "the file is truncated on disk"
    );
    assert_eq!(
        p.bound_key("i1"),
        Some(a.public_key()),
        "records before the tear are intact"
    );
    assert_eq!(p.bound_key("i2"), Some(b.public_key()));

    // The next append lands on a clean boundary and survives another restart.
    register(&mut p, &clock, &c, "i3").unwrap();
    drop(p);
    let p = open(dir.path(), &clock);
    assert_eq!(p.recovery().bytes_dropped, 0);
    assert_eq!(p.bound_key("i3"), Some(c.public_key()));
    assert_eq!(p.bound_key("i1"), Some(a.public_key()));
}

#[test]
fn a_flipped_byte_inside_a_record_is_caught_by_the_crc_and_nothing_after_it_is_trusted() {
    let dir = tempdir("bitflip");
    let clock = ManualClock::new(START);
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    let first_len;
    {
        let mut p = open(dir.path(), &clock);
        register(&mut p, &clock, &a, "i1").unwrap();
        first_len = std::fs::metadata(dir.path().join(LOG_FILE)).unwrap().len();
        register(&mut p, &clock, &b, "i2").unwrap();
    }
    let log = dir.path().join(LOG_FILE);
    let mut bytes = std::fs::read(&log).unwrap();
    let idx = first_len as usize + 8 + 3; // inside the second record's payload
    bytes[idx] ^= 0xff;
    std::fs::write(&log, &bytes).unwrap();

    let p = open(dir.path(), &clock);
    assert!(p.recovery().bytes_dropped > 0);
    assert_eq!(p.bound_key("i1"), Some(a.public_key()));
    assert_eq!(
        p.bound_key("i2"),
        None,
        "a damaged record must not be half-believed"
    );
}

#[test]
fn a_crc_valid_record_this_version_cannot_decode_refuses_to_start() {
    let dir = tempdir("unknown-record");
    std::fs::create_dir_all(dir.path()).unwrap();
    let path = dir.path().join(LOG_FILE);
    {
        let w = sangdb_log::FileWriter::open_append(&path).unwrap();
        let (mut log, _) =
            sangdb_log::Log::open(w, path.clone(), sangdb_log::FsyncPolicy::EveryRecord).unwrap();
        log.append(&[0xEE, 1, 2, 3]).unwrap();
    }
    let err = store::open(dir.path())
        .err()
        .expect("must not silently skip an identity record");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn the_control_config_debug_output_does_not_contain_the_dialkey() {
    let cfg = ControlConfig::new("super-secret-dialkey-value", vec![]);
    assert!(!format!("{cfg:?}").contains("super-secret"));
}
