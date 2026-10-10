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
use relayfold_control::conformance::{check, check_all, Property};
use relayfold_control::liveness::{self, LivenessEvent};
use relayfold_control::Stages;

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
    let mut table = Vec::new();
    for m in mutants() {
        let dir = tempdir("nc-mutant");
        let violations = check_all(m.stages, dir.path());
        let props: Vec<Property> = violations.iter().map(|v| v.property).collect();
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
