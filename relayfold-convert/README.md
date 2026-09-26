# relayfold-convert

relayfold-convert extracted from fold monorepo. Ptyktos primitives underneath.

## What it is

relayfold-convert is part of the Ptyktos substrate — a Rust-native stack extracted from
the Ptyktos fold monorepo and now published as a stand-alone crate.

## Build

```sh
cargo build --release
```

## Surfaces

| Surface | Where |
|---|---|
| Crate root | `src/lib.rs` |
| Binary (if any) | `src/bin/<bin>` |

## Integration

Pin via Cargo:

```toml
[dependencies]
relayfold-convert = { path = "../relayfold-convert" }
```

Ptyktos substrate: crates.io: `relayfold-convert`

## License

SPDX: `MIT`. Ptyktos substrate (fold-extracted).
