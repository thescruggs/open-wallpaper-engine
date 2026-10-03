# ADR 0005: the Workshop worker holds the Steam credential

- Status: accepted (maintainer, 2026-10-03)
- Date: 2026-10-03

## Context

Browsing the Steam Workshop and subscribing from the manager needs a
signed-in Steam Web session. `ARCHITECTURE.md` previously said that no
worker receives Steam credentials, and planned an optional Steamworks
bridge. The Steamworks SDK is proprietary and unnecessary: Steam's QR
sign-in (`IAuthenticationService`) yields a refresh token that
authorizes `IPublishedFileService` queries and subscriptions.

## Decision

- One dedicated process, `kwe-workshop-worker`, performs all Steam
  network access and is the only reader and writer of the credential
  file `<daemon state>/steam/steam-auth-v1.json` (directory 0700, file
  0600, versioned, quarantined when unreadable).
- The daemon stays network-free. It spawns the worker once per
  operation (cleared environment, resource limits, 40 s wall clock,
  bounded input and output) and never reads the credential file.
- No RPC result and no log line carries a token, cookie, challenge URL
  or login request id. The manager receives only the QR challenge URL
  it must display.
- The rule in `ARCHITECTURE.md` now reads: no renderer, parser, audio
  or display worker receives Steam credentials.
- The 2026-08-29 choice of the `steamguard` crate is withdrawn. It is
  protobuf-only, cannot cancel or follow a rotating QR challenge through
  its high-level API, and would add an async runtime. The worker makes
  plain form/JSON calls with a small blocking HTTP client instead,
  following the protocol used by the upstream waywallen plugin
  (credited in `THIRD_PARTY.yml`; independently implemented).

## Consequences

- A stolen state directory contains an account credential. It can be
  revoked by signing out in the manager or from Steam's
  authorized-devices page; the manager says so.
- The worker is the project's first network-facing code and brings its
  first HTTP and TLS dependencies; they are confined to that binary.
- Every Workshop operation pays a process start and a TLS handshake.
- The flow depends on partly undocumented Valve endpoints. Tests replay
  scripted responses; a shape change fails closed to the signed-out
  state.
