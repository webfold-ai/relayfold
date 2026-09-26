//! `relayfold-convert <nginx|haproxy> <input-file>` — reads the input config, converts it, and
//! prints the resulting relayfold YAML config to stdout. A conversion error prints to stderr and
//! exits non-zero, never emits a partial or guessed config on failure.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let [_, format, path] = args.as_slice() else {
        eprintln!("usage: relayfold-convert <nginx|haproxy> <input-file>");
        return ExitCode::FAILURE;
    };
    let input = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read {path:?}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match format.as_str() {
        "nginx" => relayfold_convert::nginx::convert(&input).map_err(|e| e.to_string()),
        "haproxy" => relayfold_convert::haproxy::convert(&input).map_err(|e| e.to_string()),
        other => {
            eprintln!("unsupported format {other:?} (expected nginx or haproxy)");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(cfg) => {
            println!("{}", cfg.to_yaml());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("conversion failed: {e}");
            ExitCode::FAILURE
        }
    }
}
