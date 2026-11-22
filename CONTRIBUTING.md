# Contributing

Open an issue before a large change. For a patch:

1. `cargo test --workspace`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo fmt --all -- --check`

A change that fixes a bug comes with a test that fails without the fix. A performance claim comes
with the workload, the machine, and a baseline measured the same way; a number without a baseline
is not accepted. Git dependencies are pinned by rev.

Contributions are licensed under Apache-2.0, the licence of this repository.
