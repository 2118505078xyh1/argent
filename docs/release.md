# Release checklist

1. `cargo test --workspace -- --test-threads=1`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo build --release --bin argent`
4. `powershell -File scripts/bench.ps1` — record size/startup in CHANGELOG.
5. `powershell -File scripts/local-4b.ps1` smoke test: greeting, follow-up,
   `/session`, `/hooks`, `/skills`, multiline paste.
6. `powershell -File scripts/install.ps1` (or `sh scripts/install.sh`), then
   reopen argent and confirm `--version` and `/about`.
7. Tag `vX.Y.Z` and attach the binary; update CHANGELOG with the zip's sha256
   (the bundle ships `SHA256SUMS` and, when `ARGENT_UPDATE_KEY` is set, a
   signed update manifest).

Crash reporting is off by default and there is no uploader in this build.
Diagnostics come from `argent logs`, `argent doctor`, and redacted audit
entries (`argent audit`). If an operator opts in externally, keep the payload
to redacted audit lines — never session bodies or environment variables.
