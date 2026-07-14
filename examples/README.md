# reqrun examples

- `quickstart.http` — a three-request flow (health check → login → token-authenticated request) showing named requests, environments, `client.test`/`client.assert` handlers and `client.global.set` chaining.
- `http-client.env.json` — the matching JetBrains-style environment file; `local` points at the demo server started by `scripts/smoke.sh`, `staging` is a placeholder to edit.
- `payload.json` — a body file referenced with `< payload.json` (verbatim) or `<@ payload.json` (with `{{variables}}` resolved).
- `body-from-file.http` — demonstrates both body-from-file forms plus `--dry-run` friendly requests against `example.test`.

Try them without any server:

```bash
cargo run -- examples/quickstart.http --env staging --dry-run
cargo run -- examples/body-from-file.http --dry-run
```

For a live run, `bash scripts/smoke.sh` builds a tiny demo API on `127.0.0.1:39642` and executes `quickstart.http --env local` against it end to end.
