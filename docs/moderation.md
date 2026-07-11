# Moderation — ban/kick (app level)

How removing someone from the community actually works, and which layer does
what. Written for the ban/kick wave; the message-level pieces (tombstones,
the lights-on mod log) are covered by their own code and wave-1 history.

## The two-layer model (plus a pending third)

Banning a member is enforced at two different places with two different
strengths — keep them straight:

1. **Client self-lock — immediate, UX only** (`leptos-app/src/ban_lock.rs`).
   Every signed-in client LiveQueries its *own* active `Ban` rows. The moment
   a ban syncs, the banned client replaces the whole UI with a full-screen
   lockout (title + the moderator's public reason) and calls
   `auth::sign_out()` after ~10 seconds ("Sign out now" available
   immediately). This is deliberately labeled UX, **not** security: the
   session token the user already holds keeps working at the durable node
   until it expires — live mid-session revocation is a framework-level gap
   (FA-1 territory, the guarded-agent follow-up).

2. **Mint gate — hard, at re-entry** (`server/src/main.rs`,
   `auth_session` → `active_ban_reason`). An actively banned user is refused
   a new ankurah session with HTTP 403 and the ban reason. Once the
   self-locked client signs out (or the old token expires), there is no way
   back in. This is the enforcement you can rely on.

3. **Account inactivation — pending (IdP leg).** A ban should eventually also
   disable the user's idp.to account, so they can't authenticate anywhere
   else either. Explicitly out of scope for this wave; the IdP team's design
   packet will cover it. The server-side hook belongs next to the mint gate;
   the SEAM note lives at the ban call site in
   `leptos-app/src/members_panel.rs` (`ban_member`).

"Kick" today is exactly "ban, then unban": there are no private or
room-scoped memberships yet, so there is nothing narrower to eject a user
from. Do not look for a room-kick — it doesn't exist on purpose.

## Policy shapes (policy.json)

```json
"ban": {
  "read": "view",
  "write": "moderate",
  "scope": [
    { "filter": "user = $jwt.sub", "applies_to": "read", "unless_privilege": "moderate" }
  ]
}
```

- **Self-readable by design:** every member passes the collection read gate
  (`view`), and the read-only scope pins non-moderators to rows where
  `user = $jwt.sub`. So the banned user sees exactly their own ban rows
  (that's what feeds the self-lock), moderators see all rows (the
  `unless_privilege` bypass — the members panel's "Banned" badges ride on
  this), and everyone else sees none. The members panel does not fake ban
  state for plain members — they genuinely can't read it.
- **Writes stay `moderate`:** only moderators/admins create bans or flip
  `active` off. Unban = `active = false`; there is no entity deletion in
  ankurah 0.9.0, so lifted bans remain as the audit trail.
- Pinned by `server/tests/policy_scope_tests.rs` (the four ban tests:
  self-visible, others-invisible, moderator bypass, member-cannot-write),
  in the same style as the wave-1 eight.
- Deployment note: the durable server loads `policy.json` at startup and
  republishes it into the `jwtpolicy` collection — a policy edit takes
  effect on server restart.

## The public log (`ModAction`)

Ban/unban are lights-on like everything else: both write a world-readable
`ModAction` row. The model now supports two target kinds —
`message: Option<Ref<Message>>` for message-targeted rows ("delete",
"restore") and `user: Option<Ref<User>>` for user-targeted rows ("ban",
"unban"); exactly one is set per row. Both are `Option` because a row only
carries the property for its own target kind, and absent properties only
read cleanly through `Option<T>` (bare types error on absent). The mod-log
panel renders user-targeted rows as "*Mod* banned *Name*" with the reason
quoted underneath.

## Who sees what, end to end

| Viewer            | Ban rows visible      | Members panel                        |
| ----------------- | --------------------- | ------------------------------------ |
| Moderator/admin   | all                   | "Banned" badges + ban/unban menu     |
| Banned member     | their own             | own "Banned" badge (briefly — the self-lock takes over) |
| Member in good standing | none            | no ban state at all                  |

Everyone sees the ban/unban entries in the public moderation log — that is
the point of lights-on moderation.
