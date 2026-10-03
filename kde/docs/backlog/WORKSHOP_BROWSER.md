# Workshop tab: browse, search, filter, subscribe — plan (not started)

Requested 2026-08-29: the Workshop tab shows nothing; the user wants to
browse the remote Steam Workshop, filter/search it, and subscribe to
items from the app.

> 2026-10-03: W0–W3 execution plan, with corrections to several
> assumptions below (no ACF watcher exists, the worker pattern, the
> `steamguard` decision, token derivation), is in
> `WORKSHOP_BROWSER_EXECUTION.md`. Where the two differ, that file wins.

## Why the tab is empty today (diagnosed, root causes confirmed)

The tab is a filter over the local catalog showing subscription states —
and subscription detection is broken by two parser bugs in
`kwe-core/src/scan.rs::read_workshop_subscriptions`:

1. **Wrong manifest path**: the code reads
   `<library>/steamapps/appworkshop_431960.acf`, but Steam writes
   `<library>/steamapps/workshop/appworkshop_431960.acf` (verified on
   the target: the file exists only under `workshop/`).
2. **Wrong key**: the code looks for a `workshopitems` object; modern
   ACFs carry `WorkshopItemsInstalled` (+ `WorkshopItemDetails`). The
   target's manifest has 92 installed entries — every local wallpaper
   IS a subscription, currently misclassified as `local`.

Also: per-item progress fields expected (`downloadedbytes`, …) do not
appear in the installed sections; active downloads live in the
manifest's pending/download sections and `WorkshopItemDetails`.

## Slices

### W0 — fix subscription detection (small, immediate value)

Fix the path (try `steamapps/workshop/appworkshop_431960.acf`, keep the
old location as a fallback for odd layouts), parse
`WorkshopItemsInstalled` + `WorkshopItemDetails` (case-insensitive, both
old and new key shapes), and derive `downloading` from the manifest's
pending sections rather than the absent per-item byte counters. Unit
tests against a fixture copied from the real ACF shape. Outcome: the
Workshop tab immediately shows all subscribed items with true states;
the existing offline cache (`workshop_cache.rs`) starts doing its job.

### W1 — Steam sign-in via QR code (medium; the idiot-proof auth)

Maintainer direction 2026-08-29: no API keys — sign in the way the
Steam client does, by scanning a QR code with the Steam mobile app.
This is Steam's first-class `IAuthenticationService` flow, and the
resulting user access token unlocks BOTH keyless browsing (W2) and
real in-app subscribe (W3) — the Steamworks SDK path is no longer
needed. Endpoints verified live from the target (2026-08-29):
`BeginAuthSessionViaQR` 200, `PollAuthSessionStatus` up,
`QueryFiles` 403 without auth (accepts `access_token`),
`IPublishedFileService/Subscribe`/`Unsubscribe` exist as POSTs.

Flow (all inside the isolated `kwe-workshop-worker`):

1. `BeginAuthSessionViaQR` → `challenge_url` + client/request ids.
2. Manager renders the challenge URL as a QR code (libqrencode —
   already installed on the target; becomes a package depends) with
   the states: waiting for scan → scanned, approve on your phone →
   signed in as `<persona>`. The QR self-refreshes when the challenge
   rotates; a Cancel returns to the signed-out card.
3. Poll `PollAuthSessionStatus` (bounded interval + session lifetime)
   until approval → long-lived `refresh_token` + account identity.
4. Store the refresh token daemon-side in a dedicated
   `steam-auth-v1.json` (0600 state dir, quarantine-on-corrupt
   posture, NEVER returned over any RPC, never logged). Mint
   short-lived access tokens via `GenerateAccessTokenForApp`; re-mint
   on 401; a dead refresh token flips the UI back to the sign-in card.
5. Sign out: delete the stored token and call `RevokeRefreshToken`
   (best effort); the UI notes tokens are also revocable from Steam's
   "authorized devices" page.

Implementation note: these endpoints speak protobuf
(`input_protobuf_encoded`). **DECIDED (maintainer, 2026-08-29): use
the `steamguard` crate** (MIT, maintained, implements exactly this QR
flow and tracks Valve's protocol drift upstream). Standard dependency
review still applies at merge time (license/provenance entry in
THIRD_PARTY, transitive-dep look). Security framing: the refresh
token is an account credential — it lives only in the worker/daemon
state, and the threat-model note goes in the docs.

Fallback if the user has no smartphone/Steam Guard app: W4 optional
credential login (`BeginAuthSessionViaCredentials` + Guard code) —
same machinery, deferred; the steam:// deep-link path also always
remains.

### W2 — remote browse/search/filter (large)

Remote catalog via `IPublishedFileService/QueryFiles` (appid 431960)
authenticated with the W1 access token: text search, tag filters
(WE's type/genre/resolution taxonomy as opaque strings), sort by
trend/recent/top-rated/subscriptions, cursor pagination, preview
image URLs, title/author/rating/size per item.

- **Where the network code lives**: the same isolated
  `kwe-workshop-worker` (audio.rs lifecycle pattern) — the daemon
  stays network-free, plasmashell untouched. Query RPCs
  (`workshop.query {search, tags, sort, cursor}`), bounded JSON (size
  caps, timeouts, retry backoff), and a bounded on-disk preview-image
  cache (state dir, LRU, size-capped) so the manager loads thumbnails
  from local files exactly as it does today.
- **Manager UI**: the Workshop page becomes two views — "Subscribed"
  (W0 list) and "Browse" (remote grid reusing WallpaperCard, search
  field, tag/sort pickers, infinite scroll via cursors, per-item
  detail with subscribe action and "open in Steam" fallback). All
  async states per AGENTS: loading, empty, offline, signed-out
  (sign-in card with the QR), rate-limited.

### W3 — subscribe/unsubscribe from the app (small once W1+W2 exist)

`IPublishedFileService/Subscribe`/`Unsubscribe` with the W1 access
token: a real in-app Subscribe button, no Steamworks SDK involved.
The running Steam client sees the account subscription and downloads
the item on its own; the existing ACF change-watching then flips the
item to "downloading → installed" in the tab automatically —
subscribe-to-wallpaper with zero user steps beyond the click.

Fallback that always remains: the steam:// deep link
(`WorkshopClient::openItem`) for signed-out use.

### W5 — polish (small)

Unsubscribe affordance on subscribed items, "update available"
surfacing from `NeedsUpdate` manifest fields, and a subscribed-count
badge on the tab.

## Order and rough effort

W0 (half a day, standalone — worth doing before everything else) →
W1 QR sign-in (1–1.5 days: protobuf auth + token store + QR UI) →
W2 browse (2 days: query RPCs + preview cache + UI) →
W3 subscribe (half a day) → W5 polish (half a day). W4 (credential
login fallback) only if someone actually lacks the mobile app.

## Open decisions for the maintainer

1. ~~Auth dependency~~ — DECIDED 2026-08-29: `steamguard` crate
   (formal THIRD_PARTY/provenance entry happens with the W1 merge).
2. Preview cache budget (suggest 256 MiB LRU in the daemon state dir).
3. Refresh-token storage: daemon state file 0600 (recommended,
   consistent with grants/settings posture) vs KWallet.
4. Whether Browse should also list non-subscribed installed items
   (steamcmd-downloaded folders) with a "subscribe to keep updated"
   hint.

## Risks

- `IAuthenticationService`/`IPublishedFileService` are Valve's own
  (not third-party) but partially undocumented surfaces; protocol
  drift is tracked by SteamKit/steamguard upstream — pin behavior with
  a recorded-response test suite and fail closed to the signed-out
  card on shape changes.
- The stored refresh token is an account credential: worker/daemon
  only, 0600, never over RPC, never logged; UI documents revocation
  (sign out here, or Steam's authorized-devices page).
- Steam Web API rate limits: bounded request rate, brief query-page
  cache, backoff on 429.
- WE tag taxonomy drift: treat tags as opaque strings from the API,
  never a hardcoded enum.
- ACF format drift (again): W0 parses both key generations and fails
  open to "local" (current behavior) rather than misreporting.
- Preview images are remote content rendered in the manager: they go
  through Qt's image decoders — cache files are size-capped and the
  fetcher enforces content-length/type bounds.
