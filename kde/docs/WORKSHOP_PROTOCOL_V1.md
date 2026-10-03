# Workshop protocol v1

Two message contracts for Steam Workshop browsing: daemon ⇄
`kwe-workshop-worker`, and manager ⇄ daemon (`workshop.*` RPCs, also
listed in `SUPERVISOR_API_V1.md`). Feature plan:
`backlog/WORKSHOP_BROWSER_EXECUTION.md`.

## Process boundaries

- `kwe-workshop-worker` is the only process that talks to Steam and the
  only process that reads or writes the Steam credential file.
- The daemon spawns the worker once per operation (one-shot, modelled
  on `kwe-daemon/src/inspect.rs`), never opens a network socket itself,
  never reads the credential file, and never returns or logs a token.
- The manager talks only to the daemon socket. `plasmashell` is not
  involved.

## 1. Daemon ⇄ worker

### Invocation

```
kwe-workshop-worker --state-dir <dir> --cache-dir <dir>
```

- `--state-dir`: private directory (0700) owned by the worker's data:
  `steam-auth-v1.json` (0600, `schema_version: 1`, quarantined to
  `.invalid-*` when unreadable, oversize or of another schema).
- `--cache-dir`: preview image cache (0700), LRU-evicted to 256 MiB.
- stdin: exactly one JSON request line, at most 64 KiB, then EOF.
- stdout: exactly one JSON response line, at most 4 MiB.
- stderr: bounded diagnostic `event=...` lines; never a token, cookie,
  challenge URL or request id.
- Exit status 0 whenever a response line was written, including error
  responses. Anything else is a worker failure.
- The daemon enforces a 40 s wall clock; the worker bounds every HTTP
  call to 15 s and every response body it reads (JSON 2 MiB, one
  preview image 4 MiB).

### Envelope

```json
{"version":1,"op":"query","params":{}}
{"version":1,"ok":true,"result":{}}
{"version":1,"ok":false,"error":"signed_out","detail":"short human text"}
```

Unknown fields in a request are rejected (`bad_request`).

Error codes: `bad_request`, `signed_out` (no usable credential, or
Steam rejected it — the stored credential is removed), `offline` (DNS,
connect or TLS failure), `timeout`, `rate_limited` (HTTP 429),
`steam_error` (other HTTP status; `detail` carries the status),
`protocol` (response did not have the expected shape), `internal`.

### Operations

| op | params | result |
|---|---|---|
| `auth.status` | `{}` | `{"signed_in":bool,"account_name":string,"steam_id":string}` — local only, no network; empty strings when signed out |
| `auth.begin` | `{}` | `{"client_id":string,"request_id":string,"challenge_url":string,"poll_interval_ms":int,"expires_in_ms":int}` |
| `auth.poll` | `{"client_id","request_id","challenge_url"}` | `{"state":S,"client_id":string,"challenge_url":string,"account_name":string}` |
| `auth.sign_out` | `{}` | `{"signed_in":false}` — removes the credential file; revocation is best effort |
| `query` | `{"search":string,"tags":[string],"sort":SORT,"page":int>=1}` | `{"items":[ITEM],"page":int,"total":int,"has_more":bool}` |
| `details` | `{"id":ID}` | `ITEM` with `description` filled |
| `previews` | `{"items":[{"id":ID,"url":string}]}` (≤ 60) | `{"files":{ID:absolute path}}` — only the ones now present in the cache |
| `subscribe` / `unsubscribe` | `{"id":ID}` | `{"accepted":true}` |

- `S`: `awaiting_scan`, `awaiting_confirmation`, `challenge_changed`
  (new `challenge_url` / `client_id` in the result), `signed_in`
  (credential stored by the worker; `account_name` set), `expired`.
- `SORT`: `trend_day`, `trend_week`, `trend_month`, `trend_year`,
  `recent`, `most_subscribed`, `top_rated`.
- `ID`: 1–20 ASCII digits, not `0`.
- `ITEM`:

```json
{"id":"123","title":"…","short_description":"…","description":"",
 "preview_url":"https://…","preview_file":"",
 "tags":["Scene","1920 x 1080"],"kind":"scene",
 "creator_id":"7656…","file_size":12345,"subscriptions":10,
 "favorited":3,"score":0.87,"time_updated":1700000000}
```

  `preview_file` is the absolute path of the cached preview when it is
  already in the cache, otherwise empty. `kind` is the lower-cased
  Wallpaper Engine type tag when one of `scene`, `video`, `web`,
  `application` is present, otherwise empty. Tags are opaque strings.
  Strings are length-capped (title 256, short description 1024,
  description 16384, tag 64, 32 tags; a query takes at most 16 tags).
- Preview URLs are fetched only over `https` from Steam's content
  hosts, must answer with an `image/*` content type, and are stored
  under a name derived from the item id and URL hash.

## 2. Manager ⇄ daemon

Same transport as every other daemon RPC (newline-delimited JSON over
the daemon Unix socket, one request per connection). `workshop.*`
calls are dispatched off the accept thread; the answer arrives on the
same connection and may take up to 45 s, so clients use a 60 s
deadline. One operation per family (`auth`, `query`, `details`,
`previews`, `mutation`) may be in flight; a second gets
`{"error":"busy"}`.

| method | params | result |
|---|---|---|
| `workshop.auth.status` | `{}` | `{"worker_available":bool,"signed_in":bool,"account_name":string}` |
| `workshop.auth.begin` | `{}` | `{"challenge_url":string,"poll_interval_ms":int,"expires_in_ms":int}` |
| `workshop.auth.poll` | `{}` | `{"state":S,"challenge_url":string,"account_name":string}` |
| `workshop.auth.cancel` | `{}` | `{}` |
| `workshop.auth.sign_out` | `{}` | `{"signed_in":false}` |
| `workshop.query` | as worker `query` | as worker `query` |
| `workshop.details` | `{"id":ID}` | `ITEM` |
| `workshop.previews` | as worker `previews` | as worker `previews` |
| `workshop.subscribe` / `workshop.unsubscribe` | `{"id":ID}` | `{"accepted":true}` |

- The daemon keeps the pending sign-in (`client_id`, `request_id`,
  current challenge, deadline) in memory only; `begin` replaces any
  pending one, `cancel` drops it, `poll` without one answers
  `{"error":"no_pending_login"}`. `S` adds nothing to the worker's set.
- Errors: `{"error":CODE,"detail":string}` with the worker's codes plus
  `busy`, `no_pending_login`, `invalid_params` (rejected by the daemon
  before any worker is spawned), `worker_unavailable` (binary missing;
  `workshop.auth.status` instead answers `worker_available:false`),
  `worker_failed` (crash, timeout, oversize or unparsable output).
- `workshop.query` fields are optional: `search` "", `tags` [], `sort`
  `trend_week`, `page` 1. Daemon bounds: page ≤ 100000, search ≤ 256
  bytes, tag 1–64 bytes, preview URL ≤ 768 bytes.
- A pending sign-in lives at most one hour.
- After a successful `subscribe`/`unsubscribe` the daemon marks the
  catalog stale so the next `catalog` call rescans.
- `catalog` rescans on its own when a Workshop manifest
  (`appworkshop_431960.acf`, either location) changed its modification
  time or size since the last scan, at most once every 2 s.
