# Contributing to reqrun

Thanks for your interest in improving reqrun. Issues, discussions and pull requests are all welcome.

## Getting started

Prerequisites: Rust 1.75 or newer (stable toolchain).

```bash
git clone https://github.com/JaydenCJ/reqrun.git
cd reqrun
cargo build
cargo test
bash scripts/smoke.sh
```

`scripts/smoke.sh` compiles a tiny std-only demo API (loopback only), runs `examples/quickstart.http` against it end to end — environments, token chaining, JUnit report, failing-assertion exit codes — and must print `SMOKE OK`. It finishes in well under a minute and needs no network.

## Before you open a pull request

1. `cargo fmt` — formatting is enforced.
2. `cargo clippy --all-targets -- -D warnings` — clippy must be clean.
3. `cargo test` — 83 unit tests and 10 CLI integration tests must pass.
4. `bash scripts/smoke.sh` — the smoke test must print `SMOKE OK`.
5. Add tests for behavior changes. Parsing, variable resolution and the handler interpreter live in pure modules (`parser`, `vars`, `json`, `url`, `script`) that are easy to unit-test; please keep it that way.

## Ground rules

- Keep dependencies at zero. reqrun is pure `std` by design; the only exception under discussion is TLS (see the roadmap), and any dependency needs a clear justification in the PR description.
- No network calls at startup, no telemetry. reqrun only ever connects to the hosts named in the user's `.http` files.
- Code comments and doc comments are written in English.
- Compatibility first: files must keep working in IntelliJ and VS Code unchanged. reqrun-specific behavior goes into CLI flags, never into new `.http` syntax that editors would choke on.
- Handler-subset gaps must fail loudly. A `client.*` construct we do not support has to produce a positioned error — never a silent pass.

## Reporting bugs

Please include the `.http` file (redact hosts/secrets if needed), the `reqrun --version` output, the exact command line, and what the server returned (`--verbose` output helps). Parser and handler bugs are much easier to fix with a minimal file that reproduces them.

## Security

If you find a security issue (e.g. something exploitable via a crafted `.http` file or HTTP response), please do not open a public issue. Use GitHub's private vulnerability reporting on this repository instead.
