# Workshop browser: execution plan for W0–W3 (2026-10-03, not started)

Companion to `WORKSHOP_BROWSER.md` (the 2026-08-29 feature plan). This
file is the working contract for the slices: what the code check on
2026-10-03 changed, the decisions the maintainer still owes, one task
contract per slice, and the token-efficient way to run them. Each slice
session reads **this file only** plus the files its contract names.

## 1. Corrections to the 2026-08-29 plan (verified against the tree)

| # | Plan assumption | What the tree / sources show | Consequence |
|---|---|---|---|
| C1 | "existing ACF change-watching flips the item to downloading → installed" | No watcher exists. `scan_installed` runs at daemon start and on the `rescan` RPC only (`kwe-daemon/src/main.rs:491,864`); the manager's 5 s poll calls `catalog`, which does not rescan. | New work in W3: manifest-mtime-triggered rescan. |
| C2 | Worker follows the "audio.rs lifecycle pattern" | That pattern is push-only: the worker dials the daemon socket; there is no request channel. The daemon accept loop is single-threaded with a 10 s deadline (`main.rs:562-586,658`). | Use the `inspect.rs` one-shot pattern, dispatched off-thread like `scene.inspect` (`main.rs:686-688,770`). |
| C3 | `steamguard` crate, MIT, implements exactly this flow | 0.18.4 is `MIT OR Apache-2.0`. It can begin a QR login, but `poll_until_tokens` blocks, cannot be cancelled and drops the rotated challenge. It is protobuf-only, has no published-file protos, and brings reqwest + tokio + hyper + rust-protobuf + rsa + phonenumber (lockfile 285 packages) into a workspace that today has no HTTP, TLS, protobuf or async runtime. | Decision D1 reopened. |
| C4 | Endpoints need protobuf | The in-tree upstream plugin does the whole flow with plain form/`input_json` calls: `waywallen/plugins/org.waywallen.open-wallpaper-engine/wallpaper_engine/{auth,session,api,subscription}.lua`. | A small blocking HTTP client is enough. |
| C5 | Access token from the QR poll is used directly | Upstream logs in as `platform_type = 2` (WebBrowser), then derives a *community* access token from the `steamLoginSecure` cookie via `login.steampowered.com/jwt/finalizelogin` + transfer URLs (`session.lua:356-402`) and sends that to `QueryFiles` and `Subscribe`. Whether the simpler `GenerateAccessTokenForApp` token is accepted is unconfirmed. | W1 starts with a live spike that settles it. |
| C6 | Worker holds the refresh token | `docs/ARCHITECTURE.md:162` says "No worker receives Steam credentials"; `:129-135` still describes a Steamworks steam-bridge; `:18-19` still says D-Bus. | ADR + ARCHITECTURE edit is part of W1. Decision D3. |
| C7 | Subscription state is an enum | It is a string (`CatalogItem.workshop_state`, `scan.rs:79`) repeated in `workshop_cache.rs:56-61`, `apply.rs:2262-2267`, `main.rs:1610-1628`, `catalogmodel.cpp:98,122-124,244`, `catalogclient.cpp:127-162`, `WallpaperCard.qml:128-140`. Apply and playlists accept only `local` and `subscribed_installed`. | W0 must not mis-derive `downloading`: it would make installed items unappliable. |
| C8 | Old-path ACF fixtures are in two daemon tests | Also in `scripts/smoke-{workshop-cache,live-apply,playlist-restart,apply}.sh`. | Keeping the old path as fallback keeps them valid; W0 does not edit them. |
| C9 | Browse grid reuses `WallpaperCard` | The card has 18 required properties bound to `CatalogModel` roles and selects through `WallpaperSelection`. There is no Workshop QML file; the tab is `GalleryPage` with `workshopView: true` (`Main.qml:34-42`). | W2 adds a dedicated `WorkshopBrowsePage.qml` + `RemoteItemCard.qml`. |
| C10 | Hardening comes with the worker pattern | `audio.rs` applies setpgid/PDEATHSIG/NO_NEW_PRIVS only. `inspect.rs` adds `env_clear` + allowlist, private HOME, rlimits, wall-clock timeout, size-capped report. The systemd unit does not restrict network. | Workshop worker copies `inspect.rs`. |

## 2. Decisions owed by the maintainer (recommendation first)

- **D1 — auth/network stack.** Recommended: hand-rolled calls over a
  small blocking HTTP client (`ureq` + rustls; confirm MSRV ≤ 1.88 and
  the transitive count at the W1 dependency review), using the upstream
  Lua as the protocol reference. Alternative: keep `steamguard` and
  write our own poll loop over its `AuthenticationClient` — still needs
  a second path for `QueryFiles`/`Subscribe`, and the heavier tree.
  This reverses the 2026-08-29 decision, so it needs an explicit yes.
- **D2 — provenance of the upstream Lua.** Recommended: use type
  `protocol-compatible` (endpoints, field names, state machine; no
  copied code), recorded in `THIRD_PARTY.yml` with the exact commit.
  The licence of that plugin directory must be read before any closer
  adaptation: the repo root `LICENSE` is GPL v2 text and `kde/` is
  `GPL-3.0-or-later`.
- **D3 — who holds the credential.** Recommended: the worker is the only
  process that touches the network and the token file
  (`<state>/steam/steam-auth-v1.json`, dir 0700, file 0600, quarantine
  on corrupt). The daemon never reads it, no RPC returns it, nothing
  logs it. Requires rewording `ARCHITECTURE.md:162`.
- **D4 — worker lifetime.** Recommended: one-shot process per operation
  (`inspect.rs` pattern), stateless; the daemon keeps the QR
  `client_id`/`request_id` in memory between `begin` and `poll`. Costs a
  TLS handshake per call; buys no long-lived network process and no new
  IPC protocol.
- **D5 — catalog refresh (C1).** Recommended: the `catalog` RPC stats the
  workshop manifests and rescans when an mtime changed, rate-limited to
  once per 2 s. No new dependency, no extra thread.
- **D6 — carried over.** Preview cache 256 MiB LRU; 0600 state file
  rather than KWallet; installed-but-unsubscribed items stay out of
  Browse for now.

W0 depends on none of these. D1–D4 gate W1.

**Maintainer, 2026-10-03:** plan accepted as recommended (D1–D6). On D2:
the open-source waywallen code may be used, provided it is called out
in the GitHub README and in the code (`Borrowed-From:` comments) and
recorded in `THIRD_PARTY.yml`. Still to settle before W1 adapts code
rather than ideas: upstream is GPL-2.0 and `kde/` is GPL-3.0-or-later,
so check whether upstream is "or later"; if it is GPL-2.0-only, the
worker either stays a clean reimplementation of the protocol or ships
as a separately licensed GPL-2.0-only binary like the OWE renderer
workers.

**W0 status 2026-10-03:** implemented on branch `workshop-w0`. The
target's manifest has no pending item, so the pending/download shape
could not be observed: W0 reports modern-manifest items as
`subscribed_installed` or `subscribed_missing` and derives `downloading`
only from the legacy shape. Capture a manifest mid-download during the
W3 manual test and add the state then.

**W1–W3 status 2026-10-03:** implemented on `SteamWorkShopBrowsing`
against `docs/WORKSHOP_PROTOCOL_V1.md`; ADR 0005 records D1/D3/D4. The
live spike (step 0 of W1) was not run: sign-in needs the maintainer's
phone. Unauthenticated calls were checked against Steam; everything
behind sign-in (token derivation, QueryFiles, Subscribe) is tested only
against scripted responses and gets its first real run from the
installed package. Not built: sign-out revocation at Steam (file is
deleted locally), author names, the `downloading` state.

**W5 status 2026-11-05:** implemented on `workshop-w5` (uncommitted,
awaiting orchestrator review). What it does and where:

- *Update available.* `kwe-core/src/scan.rs` compares an item's
  `latest_manifest` in `WorkshopItemDetails` with the installed `manifest`
  (`WorkshopItemsInstalled` first), with `latest_timeupdated` >
  `timeupdated` as the fallback when there are no manifest ids. The target's
  real manifest has both fields on every item (checked 2026-11-05, key names
  only; all up to date that day). `NeedsUpdate` is app-wide and not used.
  New additive catalog fields `workshop_update_available` (item) and
  `stats.update_available` (`WORKSHOP_PROTOCOL_V1.md` section 3). Display
  only: it never changes `workshop_state`, so apply and playlists are
  unaffected (C7). Shown as icon `update-low-symbolic` + "Update available"
  on the card (both galleries), a count in the gallery heading row, and an
  information message in the details pane.
- *Unsubscribe.* W3 had it only in the Browse detail sheet. The details pane
  of both galleries now offers Unsubscribe for any subscribed item
  (installed, missing, downloading) when signed in, behind a confirmation
  (Steam deletes the files). It reuses `RemoteCatalogModel::unsubscribe`,
  which already tracked pending changes by id, plus a new `pendingIds`
  property for items that are not Browse rows. Signed out, a hint points to
  the Browse sign-in or the Steam client. No new RPC.
- *Count badge.* The Workshop navigation tab reads "Workshop (N)" with N =
  installed subscriptions (the Subscribed heading's number); its tooltip adds
  the update count. Text rather than `Kirigami.Badge`: that type needs
  Kirigami 6.25 and a custom `NavigationTabBar.delegate` (6.23), and the
  package does not pin a Kirigami version.
- Not verified live: a real Steam update cycle (manifest gaining a newer
  `latest_manifest`, then Steam updating) and a live unsubscribe from the
  details pane.

**W5 post-review note (OpenRGB review fixes C2, 2026-11-06, uncommitted):**

- R8 unsubscribe never registered as finished — CLOSED. After Steam accepted
  `workshop.unsubscribe` the offline metadata cache still held the id and
  synthesized it as `subscribed_missing` on the next scan (and kept touching
  it, so it would never have aged out); the pane showed "Unsubscribing…" for
  120 s, then "Steam has not removed the item yet", with Unsubscribe offered
  again. The daemon now drops the id from the cache and records it as
  unsubscribed (`WORKSHOP_PROTOCOL_V1.md` §2); re-subscribe works, a failed
  unsubscribe leaves the cache alone. The Browse-view Unsubscribe had the
  same defect (same RPC and the same `RemoteCatalogModel` pending logic) and
  is fixed by the same change. Tests
  `workshop::tests::unsubscribe_is_not_resurrected_by_the_offline_cache`
  (confirmed failing with the fix removed),
  `workshop_cache::tests::unsubscribe_tombstone_settles_and_ages_out`.
- R9 confirmation dialog emitted `accepted` twice — CLOSED. The footer box
  forwarded `accepted`/`rejected` to the Dialog, which already follows the
  box's roles. The dialog is now `qml/UnsubscribeDialog.qml` without the
  forwarding; ctest `kwe-unsubscribe-dialog-qmltest` (qmltestrunner,
  offscreen) proves Unsubscribe → one `accepted`, Cancel / Escape → one
  `rejected` and nothing sent (confirmed failing 2× / 2× with the old
  handlers). The F4 issue-report dialog had the same forwarding (so its
  `onAccepted` would run twice; reasoned from the same Qt behaviour, not
  separately tested) and lost it too.
- NIT notice scope — CLOSED. `RemoteCatalogModel.noticeId` names the item a
  notice is about; the details pane shows a notice only for its own item.
- Still not verified live: an unsubscribe against real Steam.

## 3. Task contracts

Common to all: branch from `kde-owe-backend`; `./scripts/check.sh` green
plus `cd build/cmake && ctest`; update `docs/SUPERVISOR_API_V1.md` for
new RPCs, `AI-Skills/BETA_PLAN.md` change log and
`AI-Skills/PROJECT_MEMORY.md` session log; no live Plasma restart; no
real Workshop ids, tokens or payloads in fixtures.

### W0 — subscription detection (0.5 day)

```text
Task:            Read Workshop subscriptions from the manifest Steam actually writes
Goal:            Workshop tab lists every subscribed item with its true state
In scope:        crates/kwe-core/src/scan.rs (read_workshop_subscriptions :592-627,
                 workshop_progress :629-650, tests :831-862)
Out of scope:    daemon, manager, smoke scripts, state-string refactor
Changes:         1. Try steamapps/workshop/appworkshop_431960.acf, then the old
                    steamapps/ location.
                 2. Accept WorkshopItemsInstalled (+ WorkshopItemDetails) and the
                    old WorkshopItems key, case-insensitive.
                 3. An id present in WorkshopItemsInstalled is subscribed_installed.
                    downloading only when the manifest shows it pending (the
                    session opens by inspecting the target's real ACF for the
                    exact pending/NeedsDownload shape, then writes a synthetic
                    fixture of that shape).
                 4. Unknown shape -> empty map (items stay local), never an error
                    that hides the catalog.
Acceptance:      cargo test -p kwe-core scan:: — new tests: new path, old-path
                 fallback, new keys, mixed case, pending item, malformed file.
                 cargo test -p kwe-daemon unchanged and green (old-path fixtures).
                 On the target: `kwe` catalog stats report the installed count
                 as subscribed_installed, none stuck in downloading.
Failure tests:   8 MiB cap and symlink refusal still hold on the new path.
Provenance:      none (own parser, own fixture).
```

### W1 — worker, sign-in, token store (2–2.5 days)

```text
Task:            Steam sign-in by QR code in an isolated network worker
Goal:            Manager shows a QR; after approval on the phone it shows
                 "Signed in as <persona>"; sign-out works
Step 0 (spike):  Throwaway script outside the tree, maintainer scans once.
                 Records sanitized responses for: BeginAuthSessionViaQR
                 (platform_type 2), PollAuthSessionStatus incl. challenge
                 rotation, token mint (GenerateAccessTokenForApp vs the
                 finalizelogin cookie path), one QueryFiles call, and
                 Subscribe + Unsubscribe on one item. Output: which token
                 works where (settles C5), and the fixture set for every
                 later test. No further live calls are needed for W1–W3 tests.
In scope:        new crates/kwe-workshop-worker (ops: auth-begin, auth-poll,
                 auth-status, sign-out); new crates/kwe-daemon/src/workshop.rs
                 (spawn per inspect.rs, off-thread dispatch, one in-flight per
                 op family, busy answer); main.rs arms workshop.auth.begin|
                 poll|cancel|status|sign_out; apps/kwe-manager workshopclient
                 (state machine) + SteamSignInCard.qml with libqrencode;
                 Cargo.toml members; both PKGBUILDs (worker install line,
                 qrencode depends); THIRD_PARTY.yml; docs/ARCHITECTURE.md +
                 ADR for D3; docs/SUPERVISOR_API_V1.md
Out of scope:    browse, subscribe, credential login (W4)
Bounds:          response size cap, 20 s per HTTP call, QR session lifetime
                 180 s, poll interval from the server, bounded retries; worker
                 env cleared, private HOME, rlimits, wall-clock timeout
Acceptance:      worker unit tests replay the recorded responses through a
                 transport trait (no network in CI): happy path, rotation,
                 cancel, expiry, HTTP 429/5xx, malformed JSON, dead refresh
                 token -> signed_out. Token file: 0600, quarantine on corrupt,
                 absent from every RPC result and every log line (test greps
                 captured stderr). Qt test for the client state machine.
UI states:       signed-out, loading, waiting for scan, approve on phone,
                 signed in, cancelled, offline, failed (with retry); text +
                 icon for each
Review:          separate adversarial pass + /security-review on the diff
```

### W2 — browse, search, filter (2.5 days)

```text
Task:            Remote Workshop catalog in the manager
In scope:        worker ops query, details, fetch-previews; daemon
                 workshop.query {search, tags, sort, page} and
                 workshop.details; preview cache module (state dir, LRU,
                 256 MiB, per-file size + content-type bounds, atomic write);
                 manager RemoteCatalogModel, WorkshopBrowsePage.qml,
                 RemoteItemCard.qml, detail sheet with "Open in Steam";
                 Main.qml gains Subscribed | Browse views
Protocol facts:  QueryFiles GET, appid 431960, page/numperpage paging,
                 query_type 0 top-rated, 1 recent, 3 trend (+days), 9 most
                 subscribed, requiredtags[n], return_tags/previews/vote_data/
                 short_description (api.lua:9-20,169-194). Tags are opaque
                 strings. Author names need a second lookup; ship ids first,
                 names as a follow-up if the spike shows it cheap.
Acceptance:      worker tests on recorded pages (empty, last page, oversized,
                 malformed, 429 backoff); cache tests (eviction order, cap,
                 corrupt file, hostile content-length); Qt model test for
                 paging and state transitions; qml-typecheck green
UI states:       loading, results, empty, offline, signed-out (sign-in card),
                 rate-limited, failed; scroll position and selection survive
                 a page load; previews decode off the GUI thread
```

### W3 — subscribe / unsubscribe and catalog refresh (1 day)

```text
Task:            Subscribe from the app and see the item arrive
In scope:        worker ops subscribe, unsubscribe (POST form: access_token,
                 publishedfileid, list_type=1, notify_client=1 —
                 subscription.lua:133-147); daemon workshop.subscribe|
                 unsubscribe; D5 mtime-triggered rescan in the catalog path;
                 Subscribe/Unsubscribe actions in the Browse detail sheet and
                 on subscribed items; signed-out fallback stays
                 WorkshopClient::openItem
Acceptance:      worker tests on recorded responses (ok, 401 -> re-mint once
                 -> retry, 401 twice -> signed_out, 429); daemon test: touching
                 the manifest makes the next catalog call rescan, and a second
                 call inside the rate window does not; manual on the target:
                 subscribe -> Steam downloads -> tab shows downloading ->
                 installed with no Rescan click
UI states:       pending, done, failed with reason, Steam not running (item
                 stays "subscribed, waiting for Steam")
```

Revised total: about 6–6.5 days (was 4–4.5): the worker pattern, the
watcher, the dedicated browse UI and the ADR were not in the old figure.

## 4. Running it at the lowest token cost

Sources: Anthropic's Claude Code docs — `costs`, `best-practices`,
`sub-agents`, `prompt-caching`, `worktrees`, `sessions`
(code.claude.com/docs/en/…), read 2026-10-03.

1. **One fresh session per slice; this file is the handoff.** Docs:
   write the spec to a file, start a clean session to implement, `/clear`
   between tasks. Name the session (`workshop-w1`) so an interrupted
   slice resumes instead of being re-explored.
2. **No re-exploration.** Section 1 and the contracts carry the
   file:line map. A slice session opens only the files in its "In
   scope" line. The AGENTS.md pre-reads are large (`BETA_PLAN.md` is
   ~50 KB, mostly change log): read its "Status at a glance" section
   and `PROJECT_MEMORY.md` only; the contract above stands in for the
   rest. Suggestion for the maintainer: move change-log entries older
   than 30 days to an archive file.
3. **Models.** Docs: Sonnet for most coding, the top model for hard
   design. W0 and W3: Sonnet, single session, no sub-agents. W1 and W2:
   orchestrate here; hand the two halves (Rust worker + daemon, then
   Qt/QML) to Sonnet sub-agents with the contract as the prompt — this
   also satisfies `AI-Skills/INSTRUCTIONS.md`. Pick model and effort at
   session start and leave them: switching mid-session invalidates the
   prompt cache.
4. **Sub-agents only where they isolate bulk.** Docs: a sub-agent does
   not lower total tokens, it keeps verbose material out of the main
   context. Use them for the implementation halves and for full
   `check.sh` runs; do edits under ~3 files inline.
5. **No agent teams, workflows or parallel worktrees.** Docs: teams
   cost roughly 7x; parallel sessions scale tokens linearly. The slices
   are sequential anyway (W1 → W2 → W3 share the worker).
6. **Keep test output out of context.** Iterate on targeted runs
   (`cargo test -p kwe-core scan::`, `cargo test -p kwe-workshop-worker`).
   Run `./scripts/check.sh > /tmp/check.log 2>&1` once per slice and read
   only the exit status and failing lines. Docs suggest a hook for the
   same filtering if this becomes routine.
7. **Record once, replay forever.** The W1 spike is the only live Steam
   work. Every later test replays its sanitized responses, so no session
   spends turns on interactive network debugging.
8. **Review on the diff only.** One separate adversarial pass per slice
   (`/code-review` at medium), plus `/security-review` for W1. Reviewers
   get the diff and the contract, not the session history.
9. **Trim session overhead.** Docs: disable MCP servers a session does
   not use. These slices need none of the connected ones (Todoist,
   Drive, Calendar, Docs, Chrome).

## 5. Order

W0 now → maintainer answers D1–D4 → W1 spike (needs the maintainer's
phone once) → W1 → W2 → W3. Package build + log commit after W0 and
after W3.
