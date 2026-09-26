//! `relayfold-chart-tool` — validates `chart/relayfold-proxy/values.yaml` against the real schema
//! `relayfold-proxy-runtime`'s env vars actually read, and renders typed environment presets.
//!
//! Uses `hashtrinity::codec::syntax` (`Surface::Yaml`) for all YAML parsing/rendering, not
//! `serde_yaml` — this workspace's standing rule (see `relayfold-agents`' own `protocol.rs` doing
//! the identical thing for JSON).
//!
//! Two subcommands:
//!   relayfold-chart-tool validate <values.yaml>   — real schema check, exits non-zero on failure
//!   relayfold-chart-tool render <dev|staging|prod> — prints a typed preset as YAML to stdout

use hashtrinity::codec::syntax::{
    field, field_num, field_str, parse_surface, render_surface, term_str, Atom, SyntaxError,
    SyntaxTerm,
};

fn nested_bu<'a>(
    term: &'a SyntaxTerm,
    key: &str,
) -> Result<&'a [(String, SyntaxTerm)], SyntaxError> {
    match field(as_fields(term)?, key) {
        Some(SyntaxTerm::Bu(fields)) => Ok(fields),
        Some(_) => Err(SyntaxError::new(format!("field {key} must be a mapping"))),
        None => Err(SyntaxError::new(format!("missing field {key}"))),
    }
}

fn as_fields(term: &SyntaxTerm) -> Result<&[(String, SyntaxTerm)], SyntaxError> {
    match term {
        SyntaxTerm::Bu(fields) => Ok(fields),
        _ => Err(SyntaxError::new("expected a mapping at the document root")),
    }
}

/// Every key `relayfold-proxy-runtime`'s `main.rs` actually reads (`LISTEN_ADDR`/`UPSTREAM_HOST`/
/// `UPSTREAM_PORT`/`UPSTREAM_SCHEME`/`LOAD_BALANCE`), traced back to the values.yaml keys the
/// chart's `templates/deployment.yaml` maps them from — this validator exists so a values.yaml
/// with a typo'd or missing key fails at `helm lint`-adjacent time, not as a crash-looping pod.
fn validate(text: &str) -> Result<(), SyntaxError> {
    let term = parse_surface(hashtrinity::codec::syntax::Surface::Yaml, text)?;
    let root = as_fields(&term)?;

    let image = nested_bu(&term, "image")?;
    field_str(image, "repository")?;
    field_str(image, "tag")?;

    let replica_count = field_num(root, "replicaCount")?;
    replica_count
        .parse::<u32>()
        .map_err(|_| SyntaxError::new("replicaCount must be a non-negative integer"))?;

    let upstream = nested_bu(&term, "upstream")?;
    field_str(upstream, "host")?;
    let upstream_port = field_num(upstream, "port")?;
    upstream_port
        .parse::<u16>()
        .map_err(|_| SyntaxError::new("upstream.port must be a valid u16"))?;
    if let Some(scheme_term) = field(upstream, "scheme") {
        let scheme = term_str(scheme_term)?;
        if scheme != "http" && scheme != "https" {
            return Err(SyntaxError::new(format!(
                "upstream.scheme must be \"http\" or \"https\", got {scheme:?}"
            )));
        }
    }

    let service = nested_bu(&term, "service")?;
    let service_port = field_num(service, "port")?;
    service_port
        .parse::<u16>()
        .map_err(|_| SyntaxError::new("service.port must be a valid u16"))?;

    if let Some(lb_term) = field(root, "loadBalance") {
        let lb = term_str(lb_term)?;
        const VALID: &[&str] = &[
            "round_robin",
            "random",
            "weighted",
            "consistent",
            "healthiest",
        ];
        if !VALID.contains(&lb) {
            return Err(SyntaxError::new(format!(
                "loadBalance must be one of {VALID:?}, got {lb:?}"
            )));
        }
    }

    Ok(())
}

fn kv(key: &str, value: SyntaxTerm) -> (String, SyntaxTerm) {
    (key.to_string(), value)
}

fn s(v: &str) -> SyntaxTerm {
    SyntaxTerm::B(Atom::Str(v.to_string()))
}

fn n(v: u32) -> SyntaxTerm {
    SyntaxTerm::B(Atom::Num(v.to_string()))
}

/// Typed environment presets — Rust struct-shaped literals here, not three hand-edited YAML
/// files that can silently drift from each other. `dev` targets a single local replica against
/// no TLS; `staging`/`prod` scale replicas and flip the upstream scheme.
fn preset(env: &str) -> Result<SyntaxTerm, String> {
    let (replicas, tag, upstream_scheme, lb) = match env {
        "dev" => (1u32, "dev", "http", "round_robin"),
        "staging" => (2u32, "staging", "https", "consistent"),
        "prod" => (4u32, "stable", "https", "healthiest"),
        other => {
            return Err(format!(
                "unknown environment {other:?} — expected dev, staging, or prod"
            ))
        }
    };
    Ok(SyntaxTerm::Bu(vec![
        kv("replicaCount", n(replicas)),
        kv("loadBalance", s(lb)),
        kv(
            "image",
            SyntaxTerm::Bu(vec![
                kv("repository", s("ghcr.io/ptyktos/relayfold-proxy")),
                kv("tag", s(tag)),
            ]),
        ),
        kv(
            "upstream",
            SyntaxTerm::Bu(vec![
                kv("host", s("upstream.internal")),
                kv("port", n(8081)),
                kv("scheme", s(upstream_scheme)),
            ]),
        ),
        kv("service", SyntaxTerm::Bu(vec![kv("port", n(8080))])),
    ]))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("validate") => {
            let Some(path) = args.get(2) else {
                eprintln!("usage: relayfold-chart-tool validate <values.yaml>");
                std::process::exit(2);
            };
            let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
                eprintln!("cannot read {path}: {e}");
                std::process::exit(2);
            });
            match validate(&text) {
                Ok(()) => println!("{path}: valid"),
                Err(e) => {
                    eprintln!("{path}: INVALID — {}", e.msg);
                    std::process::exit(1);
                }
            }
        }
        Some("render") => {
            let Some(env) = args.get(2) else {
                eprintln!("usage: relayfold-chart-tool render <dev|staging|prod>");
                std::process::exit(2);
            };
            match preset(env) {
                Ok(term) => println!(
                    "{}",
                    render_surface(&term, hashtrinity::codec::syntax::Surface::Yaml)
                ),
                Err(msg) => {
                    eprintln!("{msg}");
                    std::process::exit(2);
                }
            }
        }
        _ => {
            eprintln!("usage: relayfold-chart-tool <validate <file> | render <env>>");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_VALUES: &str = r#"
replicaCount: 2
loadBalance: consistent
image:
  repository: ghcr.io/ptyktos/relayfold-proxy
  tag: v0.1.0
upstream:
  host: upstream.internal
  port: 8081
  scheme: https
service:
  port: 8080
"#;

    #[test]
    fn a_complete_real_values_file_validates() {
        validate(VALID_VALUES).expect("this shape must validate");
    }

    #[test]
    fn a_missing_upstream_port_is_rejected() {
        let broken = VALID_VALUES.replace("  port: 8081\n", "");
        let err = validate(&broken).unwrap_err();
        assert!(
            err.msg.contains("port"),
            "error should name the missing field, got: {}",
            err.msg
        );
    }

    #[test]
    fn a_non_numeric_replica_count_is_rejected() {
        let broken = VALID_VALUES.replace("replicaCount: 2", "replicaCount: many");
        assert!(validate(&broken).is_err());
    }

    #[test]
    fn an_invalid_upstream_scheme_is_rejected_not_silently_accepted() {
        let broken = VALID_VALUES.replace("scheme: https", "scheme: ftp");
        let err = validate(&broken).unwrap_err();
        assert!(err.msg.contains("scheme"), "got: {}", err.msg);
    }

    #[test]
    fn an_invalid_load_balance_algorithm_is_rejected() {
        let broken = VALID_VALUES.replace("loadBalance: consistent", "loadBalance: quantum");
        assert!(validate(&broken).is_err());
    }

    #[test]
    fn each_of_the_three_presets_renders_and_revalidates() {
        for env in ["dev", "staging", "prod"] {
            let term = preset(env).unwrap();
            let yaml = render_surface(&term, hashtrinity::codec::syntax::Surface::Yaml);
            validate(&yaml).unwrap_or_else(|e| {
                panic!(
                    "{env} preset must validate against its own schema: {}",
                    e.msg
                )
            });
        }
    }

    #[test]
    fn an_unknown_environment_name_is_a_real_error() {
        assert!(preset("qa").is_err());
    }
}
