//! Negative controls for the identity derivation (`public_key_of` / `transport::dialkey::peer_id`).
//!
//! `conformance::check_identity` is ONE function over an `Identity` (the two derivations). It is
//! run on `Identity::REAL` (must report nothing) and on mutants built by wrapping or altering the
//! real functions — each mutant calls the real `public_key_of` / `peer_id` and perturbs the result
//! — and each must be rejected for the property it breaks. The "consistent" mutants move both
//! derivations together, so the equality probe alone passes them; only the pinned golden vector
//! (and the documented-derivation probe) catches those.

use relayfold_control::auth::{public_key_of, sha256};
use relayfold_control::conformance::{check_identity, check_identity_ok, Identity, Property};

struct Mutant {
    name: &'static str,
    identity: Identity,
    /// Properties the checker must report (every one of them).
    caught_by: &'static [Property],
}

fn real_peer_id(k: &str) -> [u8; 32] {
    transport::dialkey::peer_id(k)
}

fn mutants() -> Vec<Mutant> {
    vec![
        Mutant {
            name: "peer_id returns the secret seed (the 0.1.x behaviour)",
            identity: Identity {
                peer_id: |k| sha256(format!("{k}:pkarr").as_bytes()),
                ..Identity::REAL
            },
            caught_by: &[
                Property::PeerIdIsPublicKey,
                Property::IdentityIsNotTheSecretSeed,
                Property::IdentityMatchesGolden,
            ],
        },
        Mutant {
            name: "peer_id = real peer_id with one bit flipped",
            identity: Identity {
                peer_id: |k| {
                    let mut p = real_peer_id(k);
                    p[0] ^= 1;
                    p
                },
                ..Identity::REAL
            },
            caught_by: &[Property::PeerIdIsPublicKey],
        },
        Mutant {
            name: "peer_id = SHA-256 of the real peer_id",
            identity: Identity {
                peer_id: |k| sha256(&real_peer_id(k)),
                ..Identity::REAL
            },
            caught_by: &[Property::PeerIdIsPublicKey],
        },
        Mutant {
            name: "public_key_of = real public_key_of with one bit flipped",
            identity: Identity {
                public_key_of: |k| {
                    let mut p = public_key_of(k);
                    p[31] ^= 0x80;
                    p
                },
                ..Identity::REAL
            },
            caught_by: &[Property::PublicKeyFollowsDerivation],
        },
        Mutant {
            name: "public_key_of collapses every dialkey to one identity",
            identity: Identity {
                public_key_of: |_| public_key_of("collapsed"),
                ..Identity::REAL
            },
            caught_by: &[Property::IdentityDistinctPerDialkey],
        },
        Mutant {
            name: "both derive from dialkey+\"x\" (consistent drift: equality still holds)",
            identity: Identity {
                public_key_of: |k| public_key_of(&format!("{k}x")),
                peer_id: |k| real_peer_id(&format!("{k}x")),
            },
            caught_by: &[
                Property::PublicKeyFollowsDerivation,
                Property::IdentityMatchesGolden,
            ],
        },
        Mutant {
            name: "both return the secret seed (equality holds, secret leaks)",
            identity: Identity {
                public_key_of: |k| sha256(format!("{k}:pkarr").as_bytes()),
                peer_id: |k| sha256(format!("{k}:pkarr").as_bytes()),
            },
            caught_by: &[Property::IdentityIsNotTheSecretSeed],
        },
    ]
}

#[test]
fn the_identity_checker_accepts_the_real_functions() {
    let violations = check_identity(Identity::REAL);
    assert!(violations.is_empty(), "real code violates: {violations:?}");
    check_identity_ok(Identity::REAL).expect("check_identity_ok agrees with check_identity");
}

#[test]
fn a_wrapper_that_returns_the_real_values_is_still_accepted() {
    // Guards against the checker rejecting anything built with struct-update syntax or a closure.
    let wrapped = Identity {
        public_key_of: |k| public_key_of(k),
        peer_id: |k| real_peer_id(k),
    };
    check_identity_ok(wrapped).expect("pass-through wrapper must be accepted");
}

#[test]
fn every_identity_mutant_is_rejected_for_the_property_it_breaks() {
    let mut table = Vec::new();
    for m in mutants() {
        let violations = check_identity(m.identity);
        let props: Vec<Property> = violations.iter().map(|v| v.property).collect();
        table.push(format!("{:<74} -> {:?}", m.name, props));
        assert!(
            !violations.is_empty(),
            "MUTANT SURVIVED: {} — the checker found nothing wrong",
            m.name
        );
        assert!(
            m.caught_by.iter().all(|p| props.contains(p)),
            "mutant {:?} was rejected, but not for {:?}; the checker reported {:?}",
            m.name,
            m.caught_by,
            props
        );
    }
    println!(
        "identity mutant -> properties reported by the checker\n{}",
        table.join("\n")
    );
}

#[test]
fn the_golden_vector_alone_catches_a_consistent_drift() {
    // The equality probe passes a mutant that moves both derivations together; the golden
    // probe must not.
    let drift = Identity {
        public_key_of: |k| public_key_of(&format!("{k}x")),
        peer_id: |k| real_peer_id(&format!("{k}x")),
    };
    let props: Vec<Property> = check_identity(drift).iter().map(|v| v.property).collect();
    assert!(!props.contains(&Property::PeerIdIsPublicKey), "{props:?}");
    assert!(
        props.contains(&Property::IdentityMatchesGolden),
        "{props:?}"
    );
}
