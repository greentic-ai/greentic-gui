# greentic-gui

Axum-based Greentic GUI runtime that serves tenant-specific GUI packs, enforces auth, injects fragments, and exposes worker/session/telemetry APIs plus a small browser SDK.

## Quick start

```bash
cargo run
```

## Installation (crates.io)

From source via crates.io:

```bash
cargo install greentic-gui --locked
```

## Installation (prebuilt binaries)

This repo publishes “binstall-ready” archives for Linux/macOS/Windows via GitHub Releases.

Stable (latest tagged release):

```bash
cargo install cargo-binstall
cargo binstall greentic-gui
```

Nightly (latest `master`):

- Download the correct archive from the GitHub Release named `Nightly` (tag `nightly`), unzip/untar, and place `greentic-gui` on your `PATH`.

Environment defaults:
- `BIND_ADDR=0.0.0.0:8080`
- `PACK_ROOT=./packs`
- `DEFAULT_TENANT=tenant-default`
- `GREENTIC_ENV=dev`
- `GREENTIC_TEAM=gui`

## Configuration (env vars)

- **HTTP/server**
  - `BIND_ADDR`: listen address (host:port).
  - `ENABLE_CORS`: `1`/`true` to enable permissive CORS (dev only).
- **Packs**
  - `PACK_ROOT`: filesystem root for packs.
  - `PACK_CACHE_TTL_SECS`: cache TTL for tenant configs (0 = disabled).
  - `GREENTIC_DISTRIBUTOR_URL`: enable distributor-backed pack loading.
  - `GREENTIC_DISTRIBUTOR_ENV`: distributor environment id (defaults to `GREENTIC_ENV`).
  - `GREENTIC_DISTRIBUTOR_TOKEN`: bearer for distributor calls.
  - `GREENTIC_DISTRIBUTOR_PACKS`: JSON mapping of pack refs (see `src/packs.rs`).
  - `GREENTIC_OCI_BEARER` or `GREENTIC_OCI_USERNAME` + `GREENTIC_OCI_PASSWORD`: auth when downloading OCI artifacts.
  - Cache clear: POST `/api/gui/cache/clear`.
- **Auth/OAuth**
  - `OAUTH_BROKER_URL` (required): broker base URL for `/auth/{provider}/start`.
  - `OAUTH_ISSUER`, `OAUTH_AUDIENCE`, `OAUTH_JWKS_URL` (required): bearer validation via greentic-oauth-sdk.
  - `OAUTH_REQUIRED_SCOPES`: comma-separated scopes (optional).
  - Fallback pages: static `/login` and `/logout` served from `assets/` if no pack overrides.
- **Sessions**
  - `REDIS_URL`: use Redis-backed session store; otherwise in-memory.
  - `SESSION_TTL_SECS`: cookie Max-Age; store expiry follows greentic-session defaults.
- **Workers**
  - `WORKER_GATEWAY_URL` (optional): endpoint for remote worker gateway; if unset, a stub backend echoes payloads.
  - `WORKER_GATEWAY_TOKEN` (optional): bearer token for the gateway.
  - `WORKER_GATEWAY_TIMEOUT_MS` (optional): HTTP timeout in milliseconds (default 5000).
  - `WORKER_GATEWAY_RETRIES` (optional): retry attempts on failure (default 2).
  - `WORKER_GATEWAY_BACKOFF_MS` (optional): backoff base delay between retries (default 200).
- **Auth fallbacks**
  - `/login` serves `assets/login.html` when no auth pack is mounted.
  - `/logout` redirects to `/auth/logout`.
  - `/unauthorized` serves `assets/unauthorized.html`.
- **Packs**
  - `/api/gui/cache/clear` clears the in-memory pack cache.
  - `/api/gui/packs/reload` clears cache and re-warms a tenant (JSON body `{ "tenant": "<id>" }`, default tenant if omitted); logs cache hit/miss counters.
- **Browser tests**
  - Run `npm install` (plus `npx playwright install --with-deps` if needed), start the server locally, then `npm run test:browser` to run Playwright against `/tests/sdk-harness`.
- **Telemetry**
  - Standard OTLP vars (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME=greentic-gui`, headers, etc.) respected via greentic-telemetry.

## Secrets workflow

- GUI surfaces pack-declared `secret_requirements` and a `pack_init_hint` path from `/api/gui/config`; consumers can show these to operators.
- On upstream missing-secret errors (runner/worker gateway/preflight), `/api/gui/worker/message` returns `error=missing_secrets` with the requirements and a remediation hint `greentic-secrets init --pack <path>` (extend similar handling to other APIs once upstreams emit structured errors).
- GUI never lists or fetches secret values; it only relays requirements and hints.

## SDK

- Source: `src/gui-sdk/index.ts`; bundled to `assets/gui-sdk.js` (global `window.GreenticGUI`).
- Build: `npm run build-sdk`
- Tests (Node): `npm run test-sdk` (smoke + simple assertions)
- Served at `/greentic/gui-sdk.js`

## Current limitations

- WorkerHost is an echo stub until greentic-interfaces-host exposes stable worker types/serde.
- No hot-reload/watchers for packs; distributor “internal” handles are treated as local paths.
- Fragment Wasmtime path requires real component artifacts; errors surface as logged placeholders.
- SDK has Node tests only (no browser harness yet).
