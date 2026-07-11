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

## The ankurah bridge — chosen: federate-and-remint

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
  "roles": { "member": ["view", "post"] },
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

## Status — implemented (2026-07-06)

Federate-and-remint is wired and deployed:

- **Server** (`server/src/main.rs`, `server/src/oidc.rs`) — `JwtAgent::new_durable`
  (+`watcher`) loading `policy.json`; `POST /auth/session` validates the idp.to ID
  token (JWKS/RS256/`iss`/`aud`/`exp`/`nonce`), upserts a `User` keyed on `oidc_sub`,
  and mints an ankurah session token. Signing key from `ANKURAH_JWT_SIGNING_KEY`
  (Secret Manager `community-jwt-signing-key` in prod; ephemeral dev key otherwise).
  `CorsLayer::permissive()` for cross-origin (RN) callers.
- **Client** (`leptos-app/src/main.rs`, `leptos-app/src/auth.rs`) — PKCE (S256)
  sign-in, `/auth/callback` code exchange, then federation to `/auth/session`; the
  chat UI is gated behind sign-in; the ephemeral `JwtAgent` syncs policy from the
  durable node (`jwtpolicy` collection) before reads/writes are allowed.
- **idp.to**: discovery, JWKS, `/oidc/authorize`, and `/oidc/token` are all live,
  and the token endpoint sends permissive CORS, so the in-browser exchange works.

Follow-ups: dev redirect-URI mismatch (issue #4 — the trunk dev server's randomized
port vs the registered `localhost:5173`); OIDC-aware e2e (the anonymous specs are
skipped for now); policy hardening (issue #3 — e.g. scope `user` writes to self).

## Status — sign-out + robustness pass (2026-07-10, idp lane)

- **Scopes**: `openid profile email roles`, unconditionally. The server requires
  the `roles` claim (strict mode), so a role-less request is a guaranteed dead
  end — there is no discovery probe and no degraded scope set. If idp.to's role
  configuration ever regresses, the authorize endpoint answers `invalid_scope`,
  which the callback surfaces as a retry-later message.
- **Nonce is REQUIRED at `/auth/session`**: the mint refuses an id_token without
  the browser-held nonce that it was minted against, making a leaked/replayed
  id_token useless at this endpoint. (Our own client always sent it; this
  tightens the contract to match.)
- **Sign-out is RP-initiated logout**: the client retains the idp.to id_token
  (localStorage `community_id_token`, same custody tier as the session token)
  and, on sign-out, clears local state then navigates through idp.to's
  `end_session_endpoint` (read from discovery at sign-out time) with
  `id_token_hint` + `post_logout_redirect_uri`, so the IdP session actually
  ends — previously the next "Sign in" click silently re-admitted within the
  IdP's session window. When discovery lacks the endpoint (or no id_token is
  held), sign-out degrades to the old local-clear + reload.
- **Sign-in failures render on the sign-in card** (`.signInError`), not just the
  console; one-time PKCE material is cleared when the callback consumes it,
  success or failure.
