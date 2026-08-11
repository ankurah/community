# Ankurah Community

The community & support chat for [Ankurah](https://ankurah.org) — a real-time,
multi-user chat built on Ankurah itself. A Leptos (Rust → WASM) frontend syncs
live over WebSockets with a Rust durable node backed by Postgres.

Deployed at **community.ankurah.org**.

Using the app? See the [user guide](docs/user-guide/) for how to sign in, chat, moderate, and use X-ray mode.

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

- **model/** — the data models every client uses. The chat ones (`User`,
  `Room`, `Message`, `Reaction`, `ReadState`, the DM trio) and the mention/URL
  scanner are defined in
  [ankurah-chat-model](https://github.com/ankurah/ankurah-chat) and re-exported
  from here, so an embedded chat surface elsewhere reads the same rows through
  the same definitions; community's own collections (moderation, notifications,
  link previews) are defined in this crate
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

## Tests

```bash
# Server + model. Two runs, because `main.rs` picks its storage engine at
# compile time and each build compiles code the other never sees.
cargo test -p community-server -p community-model
cargo test -p community-server --no-default-features --features sled

# SPA unit tests. A real browser, not node — they build MessageEvents and call
# encodeURIComponent. Run from the crate directory: its .cargo/config.toml
# carries the getrandom backend cfg the wasm build needs.
cd leptos-app && wasm-pack test --headless --chrome

# End to end.
cd e2e
npm install
npm run test:e2e     # picks free ports, runs Playwright (chat + multi-user)
```

CI runs all of those, and two gates besides: `cargo clippy ... -- -D warnings`
over each of the three build configurations, and `cargo audit` over
`Cargo.lock`. The audit's ignore list lives in `.cargo/audit.toml`, and an entry
there carries the reasoning that put it there.

## Deployment

The durable node runs as one pod in the shared Google Kubernetes Engine
Autopilot cluster, backed by its own database and credentials in Cloud SQL
Postgres. The web client is served same-origin from the same container. See
[`infra/gke/README.md`](infra/gke/README.md) for the deployment boundaries and
bootstrap contract.

## License

MIT or Apache-2.0
