//! Signed config push.
//!
//! The control plane signs every config it pushes with a control-plane key (derived from a
//! dialkey given at startup). The body delivered to an instance's `/relayfold-control/config` is a
//! [`SignedConfig`] envelope; the instance checks it with [`verify_config`] — a pure function an
//! agent calls with the control plane's public key, its own id, and the highest `config_version`
//! it has applied. It rejects:
//!
//! * **unsigned** configs (empty `signature`) and bodies that are not an envelope at all;
//! * configs signed by the **wrong key**, and **tampered** ones (target, version and config text
//!   are all inside the signed bytes);
//! * configs signed **for another instance** (`target` is signed, so a config captured on its way
//!   to instance A cannot be replayed to instance B);
//! * **old** configs: `config_version` must be strictly greater than the last one applied, so
//!   replaying a previously valid push is refused.
//!
//! `config_version` is a single control-plane counter: it is persisted (`Record::ConfigPushed`)
//! *before* delivery, so it is strictly increasing across restarts. A push to `"*"` carries one
//! version, signed separately for each target.

use crate::auth::{hex_decode, hex_encode, Key, Signer};
use crate::json::Json;
use crate::stages::Stages;
use wirefold::Term;

const CONFIG_DOMAIN: &[u8] = b"relayfold-control/config/v1";

/// The envelope delivered to an instance.
#[derive(Clone, Debug, Term)]
pub struct SignedConfig {
    pub target: String,
    pub config_version: i64,
    pub config: String,
    /// Lowercase hex of the 64-byte Ed25519 signature over [`config_message`].
    pub signature: String,
}

fn put(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

/// The exact bytes a config signature covers.
pub fn config_message(target: &str, config_version: u64, config: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + config.len());
    put(&mut out, CONFIG_DOMAIN);
    put(&mut out, target.as_bytes());
    put(&mut out, &config_version.to_be_bytes());
    put(&mut out, config.as_bytes());
    out
}

/// Build the signed body for one instance.
pub fn sign_config(control: &Signer, target: &str, config_version: u64, config: &str) -> Vec<u8> {
    let sig = control.sign_raw(&config_message(target, config_version, config));
    Json(SignedConfig {
        target: target.to_string(),
        config_version: i64::try_from(config_version).unwrap_or(i64::MAX),
        config: config.to_string(),
        signature: hex_encode(&sig),
    })
    .encode()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Not a config envelope (plain text, bad JSON, missing fields, negative version).
    Malformed,
    /// Envelope with an empty signature.
    Unsigned,
    /// Signature does not verify under the control-plane key (wrong key, or tampered).
    BadSignature,
    /// Validly signed, but for a different instance.
    WrongTarget,
    /// `config_version` is not greater than the last one applied.
    StaleVersion { got: u64, last_applied: u64 },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Malformed => f.write_str("not a signed config envelope"),
            ConfigError::Unsigned => f.write_str("config is unsigned"),
            ConfigError::BadSignature => f.write_str("config signature does not verify"),
            ConfigError::WrongTarget => f.write_str("config was signed for another instance"),
            ConfigError::StaleVersion { got, last_applied } => {
                write!(f, "config_version {got} is not newer than {last_applied}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedConfig {
    pub config_version: u64,
    pub config: String,
}

/// The real version rule: strictly newer than the last applied.
pub fn version_is_newer(pushed: u64, last_applied: u64) -> bool {
    pushed > last_applied
}

pub fn target_matches(signed_for: &str, this_instance: &str) -> bool {
    signed_for == this_instance
}

/// Instance-side verification. Pure: no clock, no I/O, no state — the caller owns
/// `last_applied` and bumps it after applying the returned config.
pub fn verify_config(
    body: &[u8],
    control_key: &Key,
    this_instance: &str,
    last_applied: u64,
) -> Result<VerifiedConfig, ConfigError> {
    verify_config_with(
        &Stages::REAL,
        body,
        control_key,
        this_instance,
        last_applied,
    )
}

/// [`verify_config`] with explicit decision points (the conformance checker's seam).
pub fn verify_config_with(
    stages: &Stages,
    body: &[u8],
    control_key: &Key,
    this_instance: &str,
    last_applied: u64,
) -> Result<VerifiedConfig, ConfigError> {
    let Json(env) = Json::<SignedConfig>::from_bytes(body).map_err(|_| ConfigError::Malformed)?;
    let version = u64::try_from(env.config_version).map_err(|_| ConfigError::Malformed)?;
    if env.signature.is_empty() {
        return Err(ConfigError::Unsigned);
    }
    let sig: [u8; 64] = hex_decode(&env.signature)
        .and_then(|v| v.try_into().ok())
        .ok_or(ConfigError::BadSignature)?;
    let msg = config_message(&env.target, version, &env.config);
    if !(stages.config_signature_ok)(control_key, &msg, &sig) {
        return Err(ConfigError::BadSignature);
    }
    if !(stages.config_target_ok)(&env.target, this_instance) {
        return Err(ConfigError::WrongTarget);
    }
    if !(stages.config_version_ok)(version, last_applied) {
        return Err(ConfigError::StaleVersion {
            got: version,
            last_applied,
        });
    }
    Ok(VerifiedConfig {
        config_version: version,
        config: env.config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::public_key_of;

    fn control() -> Signer {
        Signer::from_dialkey("config-test-control")
    }

    #[test]
    fn a_signed_config_verifies() {
        let body = sign_config(&control(), "proxy-1", 3, "upstream: a");
        let ok = verify_config(&body, &public_key_of("config-test-control"), "proxy-1", 2).unwrap();
        assert_eq!(ok.config_version, 3);
        assert_eq!(ok.config, "upstream: a");
    }

    #[test]
    fn unsigned_and_non_envelope_bodies_are_rejected() {
        let key = public_key_of("config-test-control");
        let unsigned = Json(SignedConfig {
            target: "proxy-1".into(),
            config_version: 9,
            config: "x".into(),
            signature: String::new(),
        })
        .encode();
        assert_eq!(
            verify_config(&unsigned, &key, "proxy-1", 0),
            Err(ConfigError::Unsigned)
        );
        assert_eq!(
            verify_config(b"upstream: 10.0.0.5:8080", &key, "proxy-1", 0),
            Err(ConfigError::Malformed)
        );
    }

    #[test]
    fn a_config_signed_by_another_key_is_rejected() {
        let body = sign_config(
            &Signer::from_dialkey("config-test-imposter"),
            "proxy-1",
            1,
            "x",
        );
        assert_eq!(
            verify_config(&body, &public_key_of("config-test-control"), "proxy-1", 0),
            Err(ConfigError::BadSignature)
        );
    }

    #[test]
    fn tampering_with_any_envelope_field_is_rejected() {
        let key = public_key_of("config-test-control");
        let good = SignedConfig::from_bytes_for_test(&sign_config(&control(), "proxy-1", 5, "a"));
        for mutated in [
            SignedConfig {
                config: "b".into(),
                ..good.clone()
            },
            SignedConfig {
                config_version: 6,
                ..good.clone()
            },
            SignedConfig {
                target: "proxy-2".into(),
                ..good.clone()
            },
        ] {
            let body = Json(mutated).encode();
            assert_eq!(
                verify_config(&body, &key, "proxy-1", 0),
                Err(ConfigError::BadSignature)
            );
        }
    }

    #[test]
    fn a_config_for_another_instance_is_rejected_even_though_validly_signed() {
        let body = sign_config(&control(), "proxy-A", 1, "x");
        assert_eq!(
            verify_config(&body, &public_key_of("config-test-control"), "proxy-B", 0),
            Err(ConfigError::WrongTarget)
        );
    }

    #[test]
    fn an_old_or_replayed_version_is_rejected() {
        let key = public_key_of("config-test-control");
        let body = sign_config(&control(), "proxy-1", 4, "x");
        assert!(verify_config(&body, &key, "proxy-1", 3).is_ok());
        assert_eq!(
            verify_config(&body, &key, "proxy-1", 4),
            Err(ConfigError::StaleVersion {
                got: 4,
                last_applied: 4
            }),
            "replay of the version just applied"
        );
        assert_eq!(
            verify_config(&body, &key, "proxy-1", 9),
            Err(ConfigError::StaleVersion {
                got: 4,
                last_applied: 9
            })
        );
    }

    impl SignedConfig {
        fn from_bytes_for_test(b: &[u8]) -> SignedConfig {
            Json::<SignedConfig>::from_bytes(b).unwrap().0
        }
    }
}
