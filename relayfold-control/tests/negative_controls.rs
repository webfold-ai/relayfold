//! Negative controls for the auth / replay / expiry / config checker.
//!
//! `conformance::check_all` is ONE function over the implementation. Here it is run on the real
//! decision points (`Stages::REAL`: must report nothing) and then on mutants — `Stages` with one
//! decision point replaced by a wrong one, everything else being the real pipeline: the real
//! `Verifier`, the real `ControlPlane`, the real log, real signatures. Each mutant must be
//! rejected, and for the property that decision point is responsible for.

mod common;

use common::tempdir;
use relayfold_control::auth::{self, NonceVerdict};
use relayfold_control::compaction::Step::{
    FsyncDir as FD, FsyncLog as FL, FsyncSnapshot as FS, RenameLog as RL, RenameSnapshot as RS,
    WriteLog as WL, WriteSnapshot as WS,
};
use relayfold_control::compaction::{Step, REAL_PLAN};
use relayfold_control::conformance::{check, check_all, Property};
use relayfold_control::liveness::{self, LivenessEvent};
use relayfold_control::Stages;

// Compaction step orders that are each one plausible way to get the protocol wrong. The real
// order is `REAL_PLAN`: snapshot written, fsynced, renamed, directory fsynced — and only then
// is the log replaced.
/// Truncates (replaces the log, durably) while the snapshot's data was never fsynced.
const TRUNCATE_BEFORE_SNAPSHOT_FSYNC: &[Step] = &[WS, RS, FD, WL, FL, RL, FD, FS];
/// Never fsyncs the snapshot at all.
const NO_SNAPSHOT_FSYNC: &[Step] = &[WS, RS, FD, WL, FL, RL, FD];
/// Replaces the log first, writes the snapshot afterwards.
const TRUNCATE_FIRST: &[Step] = &[WL, FL, RL, FD, WS, FS, RS, FD];
/// Writes a durable snapshot and never replaces the log.
const SNAPSHOT_ONLY: &[Step] = &[WS, FS, RS, FD];
/// Replaces the log and never writes a snapshot.
const TRUNCATE_ONLY: &[Step] = &[WL, FL, RL, FD];

struct Mutant {
    name: &'static str,
    stages: Stages,
    /// The property that decision point guards; the checker must report it.
    caught_by: Property,
}

fn mutants() -> Vec<Mutant> {
    vec![
        Mutant {
            name: "skips the request signature check",
            stages: Stages {
                signature_ok: |_, _, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ForgedSignatureRejected,
        },
        Mutant {
            name: "skips the request signature check (tampered body)",
            stages: Stages {
                signature_ok: |_, _, _| true,
                ..Stages::REAL
            },
            caught_by: Property::TamperedBodyRejected,
        },
        Mutant {
            name: "rejects every signature",
            stages: Stages {
                signature_ok: |_, _, _| false,
                ..Stages::REAL
            },
            caught_by: Property::ValidRequestAccepted,
        },
        Mutant {
            name: "hashes an empty body instead of the real one",
            stages: Stages {
                body_hash: |_| auth::sha256(b""),
                ..Stages::REAL
            },
            caught_by: Property::ValidRequestAccepted,
        },
        Mutant {
            name: "drops the nonce check",
            stages: Stages {
                nonce_fresh: |_, _, _, _, _| NonceVerdict::Fresh,
                ..Stages::REAL
            },
            caught_by: Property::ReplayRejected,
        },
        Mutant {
            name: "fails open when the nonce window is full",
            stages: Stages {
                nonce_fresh: |w, k, n, ts, now| match auth::nonce_fresh(w, k, n, ts, now) {
                    NonceVerdict::Full => NonceVerdict::Fresh,
                    v => v,
                },
                ..Stages::REAL
            },
            caught_by: Property::NonceWindowBounded,
        },
        Mutant {
            name: "drops the timestamp check",
            stages: Stages {
                timestamp_ok: |_, _, _, _| true,
                ..Stages::REAL
            },
            caught_by: Property::StaleTimestampRejected,
        },
        Mutant {
            name: "accepts future-dated requests (one-sided skew)",
            stages: Stages {
                timestamp_ok: |now, ts, skew, nb| ts >= nb && now - ts <= skew,
                ..Stages::REAL
            },
            caught_by: Property::FutureTimestampRejected,
        },
        Mutant {
            name: "forgets the not-before floor (pre-restart replay)",
            stages: Stages {
                timestamp_ok: |now, ts, skew, _| (now - ts).abs() <= skew,
                ..Stages::REAL
            },
            caught_by: Property::PreBootRequestRejected,
        },
        Mutant {
            name: "accepts any key on heartbeat",
            stages: Stages {
                may_heartbeat: |_, _| true,
                ..Stages::REAL
            },
            caught_by: Property::WrongKeyHeartbeatRejected,
        },
        Mutant {
            name: "lets a second key re-register (rebind) a bound id",
            stages: Stages {
                may_reregister: |_, _| true,
                ..Stages::REAL
            },
            caught_by: Property::RebindRejected,
        },
        Mutant {
            name: "never expires",
            stages: Stages {
                liveness_fold: |s, e, ttl| match e {
                    LivenessEvent::Expired { .. } => s,
                    _ => liveness::fold(s, e, ttl),
                },
                ..Stages::REAL
            },
            caught_by: Property::ExpiresAfterTtl,
        },
        Mutant {
            name: "expires immediately (never healthy)",
            stages: Stages {
                liveness_fold: |s, e, ttl| match e {
                    LivenessEvent::Expired { .. } => liveness::Liveness { expired: true, ..s },
                    _ => liveness::fold(s, e, ttl),
                },
                ..Stages::REAL
            },
            caught_by: Property::HealthyInsideTtl,
        },
        Mutant {
            name: "ignores the healthy flag in heartbeats",
            stages: Stages {
                liveness_fold: |s, e, ttl| match e {
                    LivenessEvent::Heartbeat { at, .. } => {
                        liveness::fold(s, LivenessEvent::Heartbeat { at, healthy: true }, ttl)
                    }
                    _ => liveness::fold(s, e, ttl),
                },
                ..Stages::REAL
            },
            caught_by: Property::UnhealthyReportHonored,
        },
        Mutant {
            name: "config: skips the signature check",
            stages: Stages {
                config_signature_ok: |_, _, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ConfigForgedSignatureRejected,
        },
        Mutant {
            name: "config: skips the signature check (wrong key)",
            stages: Stages {
                config_signature_ok: |_, _, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ConfigWrongKeyRejected,
        },
        Mutant {
            name: "config: skips the signature check (tampered)",
            stages: Stages {
                config_signature_ok: |_, _, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ConfigTamperedRejected,
        },
        Mutant {
            name: "config: ignores config_version (accepts replay of an old config)",
            stages: Stages {
                config_version_ok: |_, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ConfigReplayRejected,
        },
        Mutant {
            name: "config: ignores the target (accepts another instance's config)",
            stages: Stages {
                config_target_ok: |_, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ConfigWrongTargetRejected,
        },
        Mutant {
            name: "config: rejects every signature",
            stages: Stages {
                config_signature_ok: |_, _, _| false,
                ..Stages::REAL
            },
            caught_by: Property::ConfigValidAccepted,
        },
        // ── gap 1: enrollment ──
        Mutant {
            name: "enrollment: skips the allowlist check",
            stages: Stages {
                enrolled: |_, _| true,
                ..Stages::REAL
            },
            caught_by: Property::UnenrolledRegisterRejected,
        },
        Mutant {
            name: "enrollment: an empty allowlist means open",
            stages: Stages {
                enrolled: |list, key| list.is_empty() || auth::in_allowlist(list, key),
                ..Stages::REAL
            },
            caught_by: Property::EmptyAllowlistIsClosed,
        },
        Mutant {
            name: "enrollment: refuses everybody",
            stages: Stages {
                enrolled: |_, _| false,
                ..Stages::REAL
            },
            caught_by: Property::ValidRequestAccepted,
        },
        Mutant {
            name: "enrollment: refuses, but does not log the refusal",
            stages: Stages {
                record_rejection: |_, _| false,
                ..Stages::REAL
            },
            caught_by: Property::RejectionIsLogged,
        },
        // ── gap 2: the signed read path ──
        Mutant {
            name: "list: skips the enrollment check",
            stages: Stages {
                may_list: |_, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ListUnenrolledRejected,
        },
        Mutant {
            name: "list: admits an empty allowlist",
            stages: Stages {
                may_list: |list, key| list.is_empty() || auth::in_allowlist(list, key),
                ..Stages::REAL
            },
            caught_by: Property::EmptyAllowlistIsClosed,
        },
        Mutant {
            name: "list: refuses everybody",
            stages: Stages {
                may_list: |_, _| false,
                ..Stages::REAL
            },
            caught_by: Property::ListEnrolledAccepted,
        },
        Mutant {
            // The original gap 2: a read path that never authenticates and serves everybody.
            name: "list: skips authentication (every caller is treated as an enrolled instance)",
            stages: Stages {
                read_auth: |_| Ok(auth::public_key_of("conformance-instance-a")),
                ..Stages::REAL
            },
            caught_by: Property::ListUnsignedRejected,
        },
        Mutant {
            name: "list: skips the signature check",
            stages: Stages {
                signature_ok: |_, _, _| true,
                ..Stages::REAL
            },
            caught_by: Property::ListForgedRejected,
        },
        Mutant {
            name: "list: drops the nonce check (a signed read can be replayed)",
            stages: Stages {
                nonce_fresh: |_, _, _, _, _| NonceVerdict::Fresh,
                ..Stages::REAL
            },
            caught_by: Property::ListReplayRejected,
        },
        // ── gap 3: compaction ──
        Mutant {
            name: "compaction: the snapshot drops the replay (nonce) set",
            stages: Stages {
                snapshot_shape: |mut s| {
                    s.nonces.clear();
                    s
                },
                ..Stages::REAL
            },
            caught_by: Property::CompactionKeepsReplayProtection,
        },
        Mutant {
            name: "compaction: the snapshot drops config_version",
            stages: Stages {
                snapshot_shape: |mut s| {
                    s.config_version = 0;
                    s
                },
                ..Stages::REAL
            },
            caught_by: Property::CompactionKeepsConfigVersion,
        },
        Mutant {
            name: "compaction: the snapshot forgets which key each id is bound to",
            stages: Stages {
                snapshot_shape: |mut s| {
                    for b in s.instances.values_mut() {
                        b.key = [0; 32];
                    }
                    s
                },
                ..Stages::REAL
            },
            caught_by: Property::CompactionKeepsIdentity,
        },
        Mutant {
            name: "compaction: the snapshot drops every instance",
            stages: Stages {
                snapshot_shape: |mut s| {
                    s.instances.clear();
                    s
                },
                ..Stages::REAL
            },
            caught_by: Property::CompactionKeepsIdentity,
        },
        Mutant {
            name: "compaction: the snapshot drops the enrollment audit trail",
            stages: Stages {
                snapshot_shape: |mut s| {
                    s.rejections.clear();
                    s
                },
                ..Stages::REAL
            },
            caught_by: Property::CompactionKeepsAudit,
        },
        Mutant {
            name: "compaction: truncates before the snapshot is fsynced",
            stages: Stages {
                compaction_plan: || TRUNCATE_BEFORE_SNAPSHOT_FSYNC,
                ..Stages::REAL
            },
            caught_by: Property::CompactionCrashSafe,
        },
        Mutant {
            name: "compaction: never fsyncs the snapshot",
            stages: Stages {
                compaction_plan: || NO_SNAPSHOT_FSYNC,
                ..Stages::REAL
            },
            caught_by: Property::CompactionCrashSafe,
        },
        Mutant {
            name: "compaction: truncates first, writes the snapshot afterwards",
            stages: Stages {
                compaction_plan: || TRUNCATE_FIRST,
                ..Stages::REAL
            },
            caught_by: Property::CompactionCrashSafe,
        },
        Mutant {
            name: "compaction: truncates and never writes a snapshot",
            stages: Stages {
                compaction_plan: || TRUNCATE_ONLY,
                ..Stages::REAL
            },
            caught_by: Property::CompactionCrashSafe,
        },
        Mutant {
            name: "compaction: writes a snapshot and never truncates the log",
            stages: Stages {
                compaction_plan: || SNAPSHOT_ONLY,
                ..Stages::REAL
            },
            caught_by: Property::CompactionShrinksLog,
        },
    ]
}

#[test]
fn the_checker_accepts_the_real_implementation() {
    let dir = tempdir("nc-real");
    if let Err(violations) = Ok::<(), Vec<_>>(()).and_then(|()| {
        let all = check_all(Stages::REAL, dir.path());
        if all.is_empty() {
            Ok(())
        } else {
            Err(all)
        }
    }) {
        panic!(
            "the real implementation violates properties:\n{}",
            violations
                .iter()
                .map(|v| format!("  - {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    check(Stages::REAL, tempdir("nc-real-2").path()).expect("check() agrees with check_all()");
}

#[test]
fn a_mutant_that_swaps_in_the_real_function_is_still_accepted() {
    // Guards against the checker rejecting anything built with struct-update syntax.
    let identity = Stages {
        signature_ok: auth::verify_ed25519,
        ..Stages::REAL
    };
    check(identity, tempdir("nc-identity").path()).expect("identity mutant must pass");
}

#[test]
fn every_mutant_is_rejected_by_the_same_checker_for_the_property_it_breaks() {
    let mutants = mutants();
    // Each mutant gets its own scratch dir and thread: the checker is fsync-bound, so the
    // mutants overlap instead of queueing behind one another's disk flushes.
    let judged: Vec<Vec<relayfold_control::conformance::Violation>> = std::thread::scope(|s| {
        let handles: Vec<_> = mutants
            .iter()
            .map(|m| {
                s.spawn(move || {
                    let dir = tempdir("nc-mutant");
                    check_all(m.stages, dir.path())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("checker thread panicked"))
            .collect()
    });
    let mut table = Vec::new();
    for (m, violations) in mutants.iter().zip(judged) {
        let mut props: Vec<Property> = Vec::new();
        for v in &violations {
            if !props.contains(&v.property) {
                props.push(v.property);
            }
        }
        table.push(format!("{:<66} -> {:?}", m.name, props));
        assert!(
            !violations.is_empty(),
            "MUTANT SURVIVED: {} — the checker found nothing wrong",
            m.name
        );
        assert!(
            props.contains(&m.caught_by),
            "mutant {:?} was rejected, but not for {:?}; the checker reported {:?}",
            m.name,
            m.caught_by,
            props
        );
    }
    println!(
        "mutant -> properties reported by the checker\n{}",
        table.join("\n")
    );
}

#[test]
fn check_is_ok_for_the_real_code_and_err_for_a_mutant_with_the_same_function() {
    let ok = check(Stages::REAL, tempdir("nc-same-ok").path());
    let bad = check(
        Stages {
            signature_ok: |_, _, _| true,
            ..Stages::REAL
        },
        tempdir("nc-same-bad").path(),
    );
    assert!(ok.is_ok());
    assert!(bad.is_err());
}

#[test]
fn a_mutant_that_swaps_in_the_real_plan_and_the_identity_snapshot_is_still_accepted() {
    // The compaction mutants are struct updates too; the checker must not reject the shape.
    let same = Stages {
        compaction_plan: || REAL_PLAN,
        snapshot_shape: |s| s,
        enrolled: auth::in_allowlist,
        may_list: auth::in_allowlist,
        ..Stages::REAL
    };
    check(same, tempdir("nc-same-plan").path()).expect("real-equivalent mutant must pass");
}

#[test]
fn the_compaction_mutant_plans_really_differ_from_the_real_plan() {
    for plan in [
        TRUNCATE_BEFORE_SNAPSHOT_FSYNC,
        NO_SNAPSHOT_FSYNC,
        TRUNCATE_FIRST,
        SNAPSHOT_ONLY,
        TRUNCATE_ONLY,
    ] {
        assert_ne!(plan, REAL_PLAN);
    }
    // The real plan fsyncs the snapshot before the log is replaced, and fsyncs the directory in
    // between: that ordering is the crash-safety argument the crash scenario checks.
    let pos = |s: Step| REAL_PLAN.iter().position(|x| *x == s).unwrap();
    assert!(pos(FS) < pos(RS));
    assert!(pos(RS) < pos(RL));
    assert!(REAL_PLAN[pos(RS) + 1] == FD);
}
