# Contributing

Issues and focused pull requests are welcome. Please keep platform-specific job
supervision outside the wake protocol unless a change demonstrates a portable
contract and executable regression coverage.

Before opening a pull request, run:

```console
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cog check --from-latest-tag
lefthook validate
```

Install the repository hooks once per clone with `lefthook install`. The
`commit-msg` hook requires Cocogitto (`cog`) on `PATH`. Do not run `cog bump`:
Release Please is the only owner of version changes, changelog updates, tags,
and GitHub Releases.

Tests should assert observable CLI, filesystem, condition, or hook behavior.
Avoid assertions over Rust source text.

Release maintainers should also follow [docs/releasing.md](docs/releasing.md).
