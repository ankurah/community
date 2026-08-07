# Authentication — current state and the idp.to OIDC plan

## Today: OIDC members, guest read-only

Both halves are live. A member signs in with idp.to — auth-code + PKCE in the
browser, then the server federates the id_token and re-mints its own session
token ("Status — implemented" below, and "the mint is ours" within it). A
visitor who has not signed in gets a read-only session instead: the client
mints one from `POST /auth/guest` on boot ("Status — guest sessions" for the
mint, "Status — the guest boot path" for the client half). Both nodes run
`JwtAgent` against `policy.json`.

The anonymous-placeholder era this document began from — a random local
`User` in `localStorage` and `PermissiveAgent` on both nodes — is gone. The
sections below started as its replacement plan and stand as the record of
what shipped.

## Target: idp.to OIDC (auth-code + PKCE, public client)

Concrete registration from the idp.to team (2026-07-05):

```
issuer        = https://id.idp.to          # note: id.  (not admin.)
discovery     = https://id.idp.to/.well-known/openid-configuration
client_id     = app_HsW5XyYWbr0KQrHZb5iejw
redirect_uris = https://community.ankurah.org/auth/callback
                http://127.0.0.1/auth/callback          # local dev — loopback IP literals
                                                        # match ANY port; localhost does not,
                                                        # so browse dev via 127.0.0.1
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

*(Superseded 2026-08-06 — kept as the record of what was sketched before the
decision. `user.read` is no longer `view`: see the guest-mode section at the
end of this file for the shipped `view`/`signed_in` split.)*

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
port vs the registration — now the port-agnostic `127.0.0.1`); OIDC-aware e2e (the anonymous specs are
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

## Status — guest sessions (2026-08-06, auth lane)

`POST /auth/guest` (#79) mints a session for a visitor who has not signed in.
The same RS256 key signs it, through the same code path `/auth/session` uses
(`mint_session_token`), so a client checks one verifying key whichever way it
got its token. What differs: `sub` is the literal `guest` and the only role is
`guest`. No IdP round-trip, no nonce, no request body — any browser may ask —
and nothing is written to storage, so a guest leaves no `User` row and no
history. The token lives two hours (`GUEST_TOKEN_TTL_HOURS`) against the
member token's twelve, because re-minting costs a guest one unattended POST
while a member pays an OIDC round-trip. The client mints once per boot and
keeps the token in memory, so a reload mints again; a tab that outlives its
token re-mints when #86 lands ("Status — the guest boot path" explains why
there is no connect-time refusal to recover from in the meantime).

**What a guest may read** is a privilege split in `policy.json`, not a rule
about guests. `view` is the anonymous tier, and what it leaves readable is
four collections: `room`, `message`, `reaction`, `linkpreview` — room names
and topics, message text and timestamps, reaction counts, link previews. Two
different refusals produce that four, and they work at different layers.

The **collection gate** refuses three outright. A new `signed_in` privilege
keys the collections a reader has to have signed in for: `user` and
`userroles` (no roster for the street) and `modaction` (moderation records are
community business). The privilege says what its name says — the bearer
completed sign-in — and the `member` floor applied at mint means every
signed-in bearer holds it and no guest does. Signed-in visibility is
unchanged; member, moderator and admin all hold it.

`view` passes the gate on the other eleven, and the **row scopes** empty seven
of them: `ban`, `readstate`, `notification`, `notificationpref`, `dmthread`,
`dmmessage`, `dmreadstate`. Each keeps the row-local scope it already had, and
those refuse a guest with no new rule written — the scopes compare `$jwt.sub`
against an entity id, the guest subject is a literal that never parses as one,
so the comparison is false, a query matches nothing and a get by id is
refused. A guest holds no `post` privilege either, so a guest writes nothing
anywhere. `server/tests/guest_policy_live_tests.rs` runs all of that against
the real policy on a real node.

**What that leaves the guest client without: author names.** `Message` carries
`user: Ref<User>` and no display name of its own, so a reader who cannot read
the `user` collection gets the text of every message and the name of nobody —
including their own would-be neighbours in the member list, which a guest does
not receive either. Ruled on #65 (2026-08-06): guest mode ships nameless
anyway, and a public name-only projection is the fast-follow. See the client
section below for what a guest actually sees.

**How the collection gate closes, stated exactly, because a future role could
open it.** `can_access_collection` passes when a caller's roles hold the read
privilege **or** the write privilege. `signed_in` closes the roster and the
mod log only because no role today holds `post`, `moderate` or `system`
without also holding `signed_in`. Add a role like `"contributor": ["view",
"post"]` and it would read the roster through `user`'s write gate — so any new
role that may write must carry `signed_in` too.
`only_signed_in_roles_reach_the_signed_in_collections` in
`server/tests/policy_scope_tests.rs` asserts exactly that, over every role in
the file, so a role that breaks it fails there rather than in production.

**Private rooms do not exist** as a feature — `Room` carries a name, a
creator, and a topic, and nothing about visibility — so every room is public
today and a guest reads all of them. The public/private question comes back
when private rooms do.

**The mint is rate limited** per client address and per instance, because a
guest has no account to ban: identity is free per session, so the ban table
has nothing to point at. Both budgets are in-memory and per-instance (the
service runs `--max-instances 1`, so today that is the whole service; a
rollout serves two revisions with a budget each). The counted address is the
LAST `X-Forwarded-For` entry — the one Google's front end appends, and the
only one no caller can write — which assumes the service stays reached
directly through Cloud Run; an external load balancer appends two entries and
would need that read moved. See `server/src/guest.rs`.

**When refusals look shared, check for a second header line.** A request that
arrives with MORE than one `X-Forwarded-For` line is counted against the
socket peer instead — in production that peer is the front end, so every such
caller lands in one budget together and they run each other out of mints.
That is deliberate: the front end appends its address to one of the lines and
the server cannot tell which, so reading either would mean counting a value
the caller chose. An operator seeing 429s that look shared across unrelated
callers should look for callers sending their own `X-Forwarded-For` header.

## Status — the guest boot path (2026-08-06, client lane)

The client now takes the session above. No stored token means
`POST /auth/guest`, connect, and mount the chat surfaces read-only; the
sign-in card is what a visitor sees only when the boot could not hand them
anything to read with. A stored member token boots exactly as it did.

**Five ways to land on that card, each with its own sentence on it**: a
refused mint, a browser that would not open IndexedDB, a websocket that never
joined the remote system, a policy row that never synced, and a sign-in the
visitor asked for that failed. None of them is a panic and none is a blank
page — that is the rule the boot is written to, because every one of them is
the browser or the network saying no rather than a fault in the code. The
globals the app reads through are set only once the whole connect has
succeeded, so nothing downstream can reach a half-built session.

**Two of those waits are bounded here rather than upstream.** Neither
`wait_system_ready` (ankurah-core 0.9.0 parks on a notification) nor the
policy sync has a timeout of its own, and the join notification only ever
arrives over the websocket — so HTTP working while the websocket does not (an
ingress pointed elsewhere, a proxy refusing the upgrade) parked the boot for
good, and the boot is what mounts the app: a white page, forever, for a
visitor who would have got the card before any of this existed.
`SYSTEM_JOIN_TIMEOUT_MS` (8s, sized for a cold handshake on a slow mobile
connection) and `POLICY_TIMEOUT_MS` (5s, one entity over a socket already up)
are what turn that into the card. Both poll a flag rather than racing a timer,
because the wasm build carries no executor offering a select.

**Nothing about a guest session is stored.** A member token lives in
localStorage because re-acquiring one costs a ceremony; a guest token costs
one unattended POST, so every load mints and a closed tab leaves nothing
behind. That is what the mint's ten-per-address-per-minute budget is sized
for; the eleventh reload inside a minute lands on the card with one sentence
saying so.

**Who is reading** is `viewer()` in `leptos-app/src/main.rs`: the `User`
entity id a member's token names, or `None`. A guest token's subject is the
literal `guest`, which is not an entity id, so this is also the one place that
could have lied — it answers `None` for the literal and logs loudly for
anything else unparseable. Everything member-shaped hangs off it: the pair
handed to `ChatContext`, the mount gates, and the actor stamped on a
`ModAction` (which refuses to write rather than record a moderator action as
the rate limiter's "Automatic" row).

**What a guest is offered.** Rooms, message text, reactions, link previews,
the room topic, the connection light and the QR code (which encodes
`location.href` and nothing else). Absent, rather than present-and-refusing:
the member roster, the moderation log, the notification bell, the
display-name editor, the account-settings link, the sign-out button, the
whole direct-message section, and x-ray. A "Sign in" button takes their
place. Authors render as **"Unknown"** with a `?` avatar — the nameless gap
above — while the avatar's colour still comes from the message's own author
ref, so the same person stays the same colour.

**Why x-ray is a member's tool** and not merely an unimportant one: the
inspector serves a message's event history, which is the text an author
edited away, and its inspect-by-id row invites probing. Nothing there escapes
the reader's own claims — a guest's inspector reads through a guest's session
— so this is not a gap being closed. It is a decision not to widen the
audience for "I edited that to take something out" from members to anyone at
all.

**A tombstone says only what it can prove.** Attribution comes from a
matching public `ModAction` row, and `modaction` is signed-in-only — so a
guest's query for one is refused, and reading that refusal as "no row exists"
would print "Removed by the author" over every moderator's removal. A query
that could not be opened, and one that has not loaded yet, both render a bare
"Removed".

**Reaching for anything else starts the ceremony.** `ankurah-chat-leptos`
refuses an anonymous reader the caret and raises the host's auth demand
instead — a press on the message box, a reaction, a reply — and that is wired
to the framed ceremony the sign-in card uses, through one shared `SignInFlow`
(`leptos-app/src/sign_in_ceremony.rs`). Raising it while a ceremony is already
open is a no-op, which the crate requires of the callback. A completed
sign-in reloads and boots as the member. A sign-in that cannot even start —
no `sessionStorage` for the one-time material — says so in a notice the app
mounts for the purpose: it is an anonymous reader's only way out of
read-only, so it must not fail in silence.

**No re-mint at connect.** ankurah's websocket handshake carries no credential
at all — every request signs itself with the claims of the context it runs
through — so a token is not presented until the first query, and one minted
milliseconds earlier cannot be stale. A token that expires under a tab left
open past two hours is real, and recovering means calling
`auth::mint_guest_token()` again and setting the session pair `ChatApp` holds.
That is #86.

**Known rough edge, in the crate rather than here.** The components resolve
author names through one shared `user` LiveQuery, and a refusal is logged
without being cached — so a guest's console collects
`Failed to create the shared members LiveQuery: AccessDenied(CollectionDenied
("user"))` roughly three or four times per rendered message row, for as long
as the session lasts. Nothing breaks; the rows render nameless as designed.
The fix belongs to `ankurah-chat-leptos` (cache the refusal per session
generation) and rides its own pin bump.

## Status — sign-out hint ownership (2026-08-06)

- **The retained id_token names the session it belongs to**: localStorage
  `community_id_token` now holds `{id_token, session_sub}`, `session_sub`
  being the `sub` (entity id) of the ankurah session `complete_sign_in`
  minted alongside it. The slot is shared across the origin's tabs and a
  concurrent sign-in overwrites it, so sign-out presents (and removes) the
  hint only when it belongs to the session being ended; a pair some other
  session retained stays in place for that session's own sign-out, and this
  one degrades to the local-clear path. A pre-pairing bare id_token is spent
  as-is, once, so browsers signed in before this change keep the idp.to half
  of their next sign-out. The ceremony's cancel compare
  (`remove_id_token_if_matches`) reads the pair and matches its `id_token`
  field, so a cancelled exchange still takes back exactly its own write.
