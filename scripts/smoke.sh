#!/usr/bin/env bash
# Smoke test: builds reqrun, then exercises the real CLI end to end —
# offline commands first (--version, --list, --dry-run, parse errors), then
# a live run of examples/quickstart.http against a tiny std-only demo API on
# 127.0.0.1:39642 (compiled here with rustc; loopback only, no network).
set -euo pipefail

cd "$(dirname "$0")/.."

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

echo "[smoke] building..."
cargo build --quiet
BIN=target/debug/reqrun

WORK=$(mktemp -d "${TMPDIR:-/tmp}/reqrun-smoke.XXXXXX")
SERVER_PID=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# --- 1. version / help sanity -------------------------------------------------
"$BIN" --version | grep -q '^reqrun 0\.1\.0$' || fail "--version mismatch"
"$BIN" --help | grep -q 'USAGE:' || fail "--help missing usage"

# --- 2. offline: list + dry-run against the shipped examples ------------------
echo "[smoke] reqrun --list examples/quickstart.http"
"$BIN" --list examples/quickstart.http > "$WORK/list.out"
grep -q 'health GET' "$WORK/list.out" || fail "--list missing health"
grep -q 'login POST' "$WORK/list.out" || fail "--list missing login"

echo "[smoke] reqrun --dry-run (staging env, no sockets)"
"$BIN" examples/quickstart.http --env staging --dry-run > "$WORK/dry.out"
grep -q 'GET /health HTTP/1.1' "$WORK/dry.out" || fail "dry-run missing request line"
grep -q 'Host: staging.example.test:8080' "$WORK/dry.out" || fail "dry-run env not applied"

echo "[smoke] body-from-file: '<' verbatim vs '<@' substituted"
"$BIN" examples/body-from-file.http --var sku=W-100 --dry-run > "$WORK/body.out"
grep -q '"sku": "{{sku}}"' "$WORK/body.out" || fail "'<' body must stay verbatim"
grep -q '"sku": "W-100"' "$WORK/body.out" || fail "'<@' body must substitute"

# A broken file must exit 2 with a positioned error.
printf 'GET http://example.test/\nBROKEN LINE\n' > "$WORK/bad.http"
if "$BIN" "$WORK/bad.http" 2> "$WORK/bad.err"; then fail "broken file accepted"; fi
grep -q 'line 2' "$WORK/bad.err" || fail "parse error lacks line number"

# --- 3. live run against the demo API on 127.0.0.1:39642 ----------------------
echo "[smoke] compiling demo server (rustc, std only)"
rustc -O scripts/smoke-server.rs -o "$WORK/smoke-server"
if "$BIN" --timeout 1s <(echo 'GET http://127.0.0.1:39642/health') >/dev/null 2>&1; then
  fail "port 39642 is already in use; stop the other process first"
fi
"$WORK/smoke-server" --max-requests 16 > "$WORK/server.log" &
SERVER_PID=$!
for _ in $(seq 1 50); do
  grep -q READY "$WORK/server.log" 2>/dev/null && break
  sleep 0.1
done
grep -q READY "$WORK/server.log" || fail "demo server did not start"

echo "[smoke] reqrun examples/quickstart.http --env local"
"$BIN" examples/quickstart.http --env local --report "$WORK/report.xml" > "$WORK/run.out"
grep -q 'PASS  health' "$WORK/run.out" || fail "health did not pass"
grep -q 'PASS  login' "$WORK/run.out" || fail "login did not pass"
grep -q 'PASS  whoami' "$WORK/run.out" || fail "token chaining (whoami) did not pass"
grep -q '3 request(s): 3 passed, 0 failed — 5 check(s)' "$WORK/run.out" \
  || fail "summary line wrong: $(tail -1 "$WORK/run.out")"

echo "[smoke] JUnit report for CI"
grep -q 'tests="3" failures="0" errors="0"' "$WORK/report.xml" || fail "junit counts wrong"
grep -q '<testcase name="whoami"' "$WORK/report.xml" || fail "junit missing testcase"

# --- 4. failing assertion must exit 1 with details -----------------------------
cat > "$WORK/failing.http" <<'EOF'
GET http://127.0.0.1:39642/health

> {% client.assert(response.body.version === '9.9.9', "version pinned"); %}
EOF
if "$BIN" "$WORK/failing.http" > "$WORK/failing.out"; then
  fail "failing assertion exited 0"
fi
grep -q 'FAIL' "$WORK/failing.out" || fail "failure not reported"
grep -q 'version pinned' "$WORK/failing.out" || fail "assertion message missing"

echo "SMOKE OK"
