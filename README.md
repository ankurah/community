# Ankurah Community

The community & support chat for [Ankurah](https://ankurah.org) — a real-time,
multi-user chat built on Ankurah itself. A Leptos (Rust → WASM) frontend syncs
live over WebSockets with a Rust durable node backed by Postgres.

Deployed at **community.ankurah.org**.

## Features

- Real-time message sync across all connected clients
- Rooms, soft-deletable messages, editable display names
- Virtual-scrolled message history (`ankurah-virtual-scroll`)
- Reactive UI (Leptos + `ankurah-signals`)
- Durable node: Postgres on the server, IndexedDB in the browser

## Quick start

The background dev runner builds and supervises the server + Leptos app on
randomized local ports (and, because this project uses Postgres, brings up a
throwaway `postgres:16` container). It publishes status files for a
[Sutra](https://github.com/synestheticsystems/sutra) dashboard.

```bash
./dev.sh            # start (prints the web URL to open)
./dev.sh --status   # status
./dev.sh --logs     # tail combined logs
./dev.sh --stop     # stop (also removes the Postgres container)
```

Requires [trunk](https://trunkrs.dev/) (`cargo install trunk`), the wasm target
(`rustup target add wasm32-unknown-unknown`), and Docker (for the Postgres
container).

## Architecture

- **model/** — shared data models (`User`, `Room`, `Message`), used by every client
- **server/** — the durable node: `ankurah-websocket-server` + Postgres storage
- **leptos-app/** — Leptos (CSR) web client, compiled to WASM with [trunk](https://trunkrs.dev/)

This repo is laid out for **multiple clients** sharing `model/` + `server/`: the
Leptos web app today, with a React Native client to be folded in later. Clients
connect to the durable node's WebSocket endpoint (same-origin in the browser; a
configurable URL for native clients).

## Models

### User
- `display_name: String`

### Room
- `name: String`

### Message
- `user: Ref<User>` (LWW) — the sender
- `room: Ref<Room>` (LWW) — the room
- `text: String` — message content
- `timestamp: i64` (LWW) — Unix milliseconds
- `deleted: bool` (LWW) — soft-delete flag

## Authentication & authorization

Sign-in is [idp.to](https://idp.to) OIDC (PKCE, passkeys). The client posts the
verified ID token to `POST /auth/session`; the server validates it against the
idp.to JWKS and re-mints an Ankurah `JwtAgent` session token
(federate-and-remint), enforced end-to-end by the policy in `policy.json`.

Roles (`member` / `moderator` / `admin`) are **owned by the IdP**: they arrive
as a required `roles` claim in the ID token (an ID token without a well-formed
roles array fails sign-in), are administered in the idp.to console, and are
minted verbatim into the session token with a `member` floor. The server keeps
a read-only `userroles` cache per user so the Members panel can show badges.
See [`docs/auth.md`](docs/auth.md).

## End-to-end tests

```bash
cd e2e
npm install
npm run test:e2e     # picks free ports, runs Playwright (chat + multi-user)
```

## Deployment

The durable node runs on Google Cloud Run (single instance, scale-to-zero) with
Cloud SQL Postgres; the web client is served same-origin from the same container.

## License

MIT or Apache-2.0
