# The `.http` grammar reqrun executes

reqrun v0.1.0 parses the JetBrains HTTP-client format (also used by the VS Code
REST Client). This page is the precise list of what is executed, ignored, or
rejected — the compatibility contract behind "the file you edit is the file CI
runs".

## Requests

```
### optional title            <- separator; the title names the next request
# @name login                 <- wins over the separator title
# @no-redirect                <- per-request: do not follow 3xx
POST http://example.test/login HTTP/1.1
Content-Type: application/json

{"user": "admin"}

> {% client.assert(response.status === 200); %}
```

- The request line is `METHOD URL [HTTP/x.y]`; a bare URL means `GET`. The
  version token is accepted and ignored — reqrun always speaks HTTP/1.1.
- Indented continuation lines starting with `?` or `&` extend the URL.
- Headers follow until a blank line; `#`/`//` comment lines between headers are
  skipped. The body runs until the next `###` (interior blank lines belong to
  the body, trailing ones are trimmed).
- `< path` sends a file verbatim; `<@ path` resolves `{{variables}}` inside it
  first. Paths are relative to the `.http` file.
- `>> path` saves the response body (`>>! path` overwrites). `<> path`
  saved-response references are ignored, like the editors' diff feature.
- `> {% ... %}` is the response handler; `> path.js` external scripts are a
  clear error (inline the script instead).

## Variables

Resolution order, first hit wins: `--var` overrides → `client.global.set`
values → file `@name = value` variables → the `--env` environment. Environments
come from `http-client.env.json` next to the file (or `--env-file`), with
`http-client.private.env.json` overriding key-by-key. Unknown variables fail
the request — except under `--dry-run`, where handler-captured values cannot
exist yet and render literally.

Dynamic variables: `{{$uuid}}`, `{{$timestamp}}`, `{{$isoTimestamp}}`,
`{{$randomInt}}`, `{{$random.integer(a, b)}}`, `{{$env.NAME}}`.

## Response handlers

The interpreted subset is documented in the README ("Supported handler
subset"). The rule of thumb: `client.test`, `client.assert`,
`client.global.set` and `client.log` over `response.*` expressions work;
arbitrary JavaScript (loops, `var`, functions, `JSON.*`) is a positioned error
so a check that cannot run never reports green.

## Known deviations from the editors

- HTTPS is not supported in v0.1.0 (std-only build; see the roadmap).
- No cookie jar yet; `Set-Cookie` headers are visible to assertions but not
  replayed.
- `multipart/form-data` boundaries and GraphQL bodies are sent as raw text —
  fine if you write the body yourself, no builder sugar yet.
- Handler scripts run after the full response is read; there is no streaming
  API.
