# Authentication — current state and the idp.to OIDC plan

## Today: anonymous placeholder

The app ships with **no real authentication**, on purpose (deploy-now,
OIDC-later):

- **Browser** (`leptos-app/src/main.rs`) — `ensure_user()` creates a random
  `User`, stores its id in `localStorage`, and the node runs
  `PermissiveAgent::new()` (every operation allowed).
- **Server** (`server/src/main.rs`) — the durable node also runs
  `PermissiveAgent::new()`.

That is the exact seam real sign-in replaces.

## Target: idp.to OIDC (auth-code + PKCE, public client)

Concrete registration from the idp.to team (2026-07-05):

```
issuer        = https://id.idp.to          # note: id.  (not admin.)
discovery     = https://id.idp.to/.well-known/openid-configuration
client_id     = app_HsW5XyYWbr0KQrHZb5iejw
redirect_uris = https://community.ankurah.org/auth/callback
                http://localhost:5173/auth/callback     # local dev
scopes        = openid profile email
```

It's a **public client with PKCE (S256)** — no client secret, and the OIDC code
exchange happens **entirely in the browser** (a static SPA does the whole dance).
`/auth/callback` is a client-side route, already served by our SPA fallback.

Flow: make a PKCE `code_verifier`/`code_challenge` → redirect to
`authorization_endpoint` (`response_type=code`, `state`, `nonce`, `code_challenge`,
`code_challenge_method=S256`) → user signs in (passkey-first; self-signup is
enabled) → callback with `?code&state` (verify `state`) → POST `token_endpoint`
with `code_verifier` (no secret) → validate the ID token: RS256 signature via
JWKS, `iss == https://id.idp.to`, `aud == client_id`, `exp`/`iat`, `nonce`.
Claims: `sub` (stable — key `User` records on it), `email`, `name`. Resolve the
endpoint URLs at runtime from discovery; re-fetch JWKS on an unknown `kid`.
Libraries: `oidc-client-ts` (JS) or the `openidconnect` crate (Rust/wasm).

## The ankurah bridge (still to design)

Getting the idp.to ID token is client-side; making ankurah **trust** it is the
open question. `ankurah-jwt-auth`'s `JwtAgent` verifies a single RS256 PEM key —
no JWKS, no `kid`, no `iss`/`aud` checks — so it can't consume idp.to's
rotation-ready JWKS directly. Two options:

1. **Federate-and-remint** — a small server route verifies the idp.to ID token
   (JWKS) and mints an ankurah `JwtAgent` session token via `SigningKeys::sign`.
   idp.to signs with the *same* `ankurah_jwt_auth::SigningKeys` primitive, so
   this is a natural fit. Needs no ankurah changes; adds one backend route.
2. **Teach `JwtAgent` external JWKS** — add issuer + JWKS verification to
   ankurah-jwt-auth so the browser token is trusted directly (no mint route).

Decide when wiring. Either way, a verified identity → `JwtContext::from_claims`
→ `node.context(ctx)`, and the durable node swaps to
`JwtAgent::new_durable(keys, "policy.json")` (see the policy sketch below).

### Policy (`policy.json`) sketch

```json
{
  "roles": { "Member": ["view", "post"] },
  "collections": {
    "message": {
      "read": "view", "write": "post",
      "scope": [ { "filter": "user = $jwt.sub", "applies_to": "write" } ]
    },
    "room": { "read": "view", "write": "post" },
    "user": { "read": "view", "write": "post" }
  }
}
```

## Status / blockers

- Discovery + JWKS + our registration: **live now** on `id.idp.to`.
- `/oidc/authorize` + `/oidc/token`: **activating shortly** (idp.to #19/#20) —
  we'll get an "it's live" ping. Safe to implement the flow now; it lights up
  with no change on our side.
- **Dev redirect mismatch:** the registered dev URI is
  `http://localhost:5173/auth/callback`, but our trunk dev server uses a
  randomized port. When wiring OIDC, pin a dev port (or have Daniel re-register
  the real one — a 30-second change on the idp.to side).

## When wiring it, touch only

- `leptos-app/src/main.rs` — replace `ensure_user()` with the PKCE sign-in +
  callback handling; map `sub` → ankurah `User` (create-on-first-login).
- `server/src/main.rs` — swap `PermissiveAgent` → `JwtAgent::new_durable`; add
  the mint route if going federate-and-remint; add `policy.json` + the ankurah
  signing key (from Secret Manager in prod). Add `CorsLayer` if any client is
  cross-origin.
