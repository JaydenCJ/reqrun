# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-13

### Added

- JetBrains-compatible `.http` parser: `###` separators with titles, `# @name` / `// @name`, `# @no-redirect`, comments, file-level `@name = value` variables, implicit-GET bare URLs, multi-line URL query continuation, headers, inline bodies, `< file` (verbatim) and `<@ file` (variable-substituted) bodies, `> {% ... %}` response handlers, `>> file` / `>>! file` response saving, CRLF tolerance, and positioned parse errors.
- Variable resolution with editor-matching precedence (`--var` > handler globals > file variables > environment), `http-client.env.json` + `http-client.private.env.json` environments with `--env` / `--env-file`, and the dynamic variables `{{$uuid}}`, `{{$timestamp}}`, `{{$isoTimestamp}}`, `{{$randomInt}}`, `{{$random.integer(a, b)}}`, `{{$env.NAME}}`.
- Zero-dependency HTTP/1.1 client on `std::net`: Content-Length and chunked bodies, read-to-EOF fallback, connect/read timeouts, redirect following with RFC 9110 method demotion, default-header injection with case-insensitive user overrides.
- Native interpreter for the response-handler subset: `client.test` (function and arrow bodies), `client.assert` (with expression-source failure messages), `client.global.set` (request/file chaining), `client.log`; expressions over `response.status` / `body` / `headers.valueOf` / `contentType`, JSON member and index access, string helpers, comparisons, `+` (numeric addition / string concatenation), `&&` / `||` / `!` — anything unsupported is a positioned error, never a silent pass.
- CLI: `--env`, `--env-file`, `--var`, `--request`, `--timeout`, `--strict`, `--fail-fast`, `--dry-run` (lenient rendering of not-yet-captured variables), `--list`, `--report` (JUnit XML), `--verbose`, `--no-color` / `NO_COLOR`, and exit codes `0` / `1` / `2`.
- Console report with per-assertion detail and a JUnit XML export where every request is a test case (failures, errors and skips mapped accordingly).
- Runnable examples (`examples/quickstart.http`, `body-from-file.http`, env file) and a loopback-only demo API for the smoke script.
- Test suite: 83 unit tests, 10 CLI integration tests against the compiled binary with an in-process server on 127.0.0.1, and `scripts/smoke.sh`.

[0.1.0]: https://github.com/JaydenCJ/reqrun/releases/tag/v0.1.0
