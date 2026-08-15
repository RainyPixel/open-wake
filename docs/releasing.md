# Releasing

Releases are tag-driven. GitHub Actions builds native binaries on four hosted
runners:

- x86_64 Linux (static musl)
- aarch64 Linux (static musl)
- x86_64 macOS
- aarch64 macOS

To release:

1. Update `package.version` in `Cargo.toml` and regenerate `Cargo.lock`.
2. Run the repository gates from `CONTRIBUTING.md`.
3. Commit and push the version change.
4. Create and push the matching annotated tag, for example `v0.2.0`.
5. Read back the Release workflow and published GitHub Release assets.

The workflow refuses a tag that does not exactly equal `v` plus the Cargo
package version. It tests on every native runner, builds the four archives,
generates one `SHA256SUMS`, and publishes the release only after every build
succeeds.

Asset names are a public updater contract:

```text
codex-wake-x86_64-unknown-linux-musl.tar.gz
codex-wake-aarch64-unknown-linux-musl.tar.gz
codex-wake-x86_64-apple-darwin.tar.gz
codex-wake-aarch64-apple-darwin.tar.gz
SHA256SUMS
```

Do not rename these without updating both `install.sh` and `src/update.rs`.
