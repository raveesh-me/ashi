# ashi

ashi is a double-entry financial transactions database written in Rust, in the
same class as TigerBeetle: a ledger that records accounts and the transfers
between them.

## Build & test

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```
