# relayfold-convert

Convert **nginx** and **HAProxy** configuration into relayfold's own declarative route format.
Two real, tested parsers — not six thin ones.

## Why this instead of the obvious alternative

Migrating a proxy usually means one of two things, and both are bad:

- **hand-editing the routes** into the new format, which is slow, unreviewable, and drifts;
- **running the old proxy next to the new one**, which means two systems to keep in step.

A parser that reads the old config and emits the new one makes the migration reviewable: the
output is a document you can read, diff and check. The format it emits is **plain YAML** — the
route format itself, not a new DSL — because a config you cannot read in a text editor is a
config nobody will review.

## What you get

| item | what it is |
|---|---|
| `config` | the route format: `ProxyConfig`, `Route`, `UpstreamCfg`, `AllowRule` |
| `nginx` | the nginx parser — exactly what it covers and what it refuses to guess at |
| `haproxy` | the HAProxy parser — same |

Each parser documents its own coverage and its refusals: a construct it does not understand is
**reported**, not silently dropped. A migration that quietly loses a route is worse than one
that fails.

## Deliberately not here — disclosed, not stubbed out

- **Apache, Traefik, Caddy and Envoy** converters. Real gaps against a six-format claim, left
  open for whoever needs one next rather than faked as coverage.
- **A loader that feeds this crate's `ProxyConfig` back into a live registry.** This crate
  depends on nothing from `relayfold` on purpose — its job is producing a correct, standalone
  config document. The loader is a small, separate function in whichever consumer actually
  builds a registry from one.

## Build

```sh
cargo build --release
```

## Integration

```toml
[dependencies]
relayfold-convert = { path = "../relayfold-convert" }
```

or from git:

```toml
[dependencies]
relayfold-convert = { git = "https://github.com/webfold-ai/relayfold", rev = "<pin>" }
```

## License

SPDX: `Apache-2.0` (declared in `Cargo.toml`).
