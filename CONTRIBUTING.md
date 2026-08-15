# Contributing

Issues and focused pull requests are welcome. Please keep platform-specific job
supervision outside the wake protocol unless a change demonstrates a portable
contract and executable regression coverage.

Before opening a pull request, run:

```console
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

Tests should assert observable CLI, filesystem, condition, or hook behavior.
Avoid assertions over Rust source text.

Release maintainers should also follow [docs/releasing.md](docs/releasing.md).
