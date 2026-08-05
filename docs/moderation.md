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

---

# Direct messages (#30) — what moderation can and cannot do

Two-party DMs change the moderation picture in one specific way, so the
posture is written down here rather than inferred from `policy.json`.

## Moderators cannot read DMs. That is the ruling, not an oversight.

The `dmthread` and `dmmessage` read scopes are `a = $jwt.sub OR b = $jwt.sub`
with **no `unless_privilege`**. A moderator who is not one of the two
participants gets nothing: not a one-shot fetch, not a live subscription, not
a get by entity id. `server/tests/dm_policy_live_tests.rs` asserts all three
against the real policy on a real node, and
`policy_scope_tests.rs::dm_scope_rule_shapes_unchanged_and_no_moderator_bypass`
fails the moment somebody adds the bypass. Both tests exist because adding
moderator visibility is a one-line policy change, and a one-line change to a
privacy posture should have to argue with a test.

**Abuse response therefore flows through reports, never through browsing.** A
member who receives an abusive DM reports the message; the report carries the
message ref, and the moderator acts on what the report contains. There is no
"open this member's conversations" affordance anywhere in the product, and
building one would mean changing the policy above, in public, on purpose.
(The report flow itself is roadmap item 2.10; until it lands, a recipient's
route is to tell a moderator directly, and the moderator's tools are the ones
that already exist — `Ban`, and the DM rate limiter below.)

The one thing moderators DO see is the public `ModAction` log, including the
automatic `dm-rate-limit` rows described next.

## The DM rate limiter — post-hoc, and honest about it

`server/src/workers/dm_rate_limit.rs`. This is the stranger-DM mitigation the
feature request asked for (a per-sender rate limit on thread creation and
first messages); the alternative it was chosen over — a "DM requests" accept
gate — is specced at #67 and waits on real abuse data.

**Where enforcement actually happens, precisely.** There is no seam in this
codebase where a remote write can be refused. The only such gate is
`check_event` inside `ankurah-jwt-auth`, which an application cannot extend
(the wrapper-agent approach was killed in an earlier architecture review). So
the limiter is a worker, and it acts *after* the fact: an offending message
commits, replicates, and reaches the recipient's client, and is then
tombstoned — usually within a second, but a recipient with the thread open can
watch a message appear and turn into "Message removed". Nothing here stops a
determined sender in the moment; it makes volume expensive and leaves a trail.
**The escalation is a human moderator issuing a `Ban`**, which is enforced at
token mint and is the part you can rely on.

**What is counted, per sender, over a trailing 60 minutes:**

| Limit                                    | Value | What trips it                                                |
| ---------------------------------------- | ----- | ------------------------------------------------------------ |
| Conversations **started**                | 5     | The sender's message is the oldest in its thread              |
| Messages into **unanswered** threads     | 20    | Threads where the other participant has never sent anything   |

A thread the correspondent has replied in is exempt from the second limit
entirely: two people talking are not a broadcast, and a real conversation
never approaches the number.

**Attribution comes from `DmMessage.user`, never from the thread row.**
`DmThread` deliberately records no creator: the write scope only checks that
the writer is one of `a`/`b`, so a `created_by` field would be unreliable — a
sender could name someone else as the creator and push THEM into a rate limit. `DmMessage.user` is pinned
to the caller by the policy's sender-binding rule, so "who started this
conversation" derived from the oldest message is the one attribution a client
cannot lie about.

**Timestamps are client-supplied and the window lives with that.** A message
dated later than the server's clock is rewritten to the server's clock, and the
rewrite is *committed* to the row, by `server/src/workers/dm_timestamp.rs`, the
first time the server sees it. That kills future-dating — a message dated next
year would otherwise sit at the top of every newest-first list forever, relight
an unread badge the reader cannot clear, and re-enter the rate limiter's window
on every restart. Persisting the clamped value matters as much as computing it:
a number each reader compensates for privately is recomputed against the
current clock, so it moves between readings, which is what produced all three
of those. Back-dating to slip out of the window is possible and is accepted: a
back-dated message buries itself in the recipient's history, so the evasion
costs the sender the visibility they wanted.

**What a breach does — and what it deliberately does not.** Either limit
tombstones one message and nothing else: over the initiation limit, the
message that opened the excess conversation; over the unanswered limit, the
message that ran the budget out. The `DmThread` row and every earlier message
in it survive.

An earlier version tombstoned the whole thread and its history on an
initiation breach. That is the one thing an automatic penalty must not do
here: nothing anywhere in this codebase writes `deleted` back to `false`, so a
single false positive permanently destroyed a two-way conversation — the other
participant's messages included — with no repair path for them, for the
sender, or for a moderator. Tombstoning the message costs a bulk sender exactly
the same delivery, because a thread whose only message was tombstoned has
nothing left to show, and threads with no messages appear in neither sidebar.
The friction survives; the data does not die.

**One** `ModAction { action: "dm-rate-limit" }` row is written per sender per
window, and it goes in *before* the tombstone: a tombstone must never outrun
the public trace that justifies it. If that write fails, nothing is tombstoned
and the sender's next message retries both halves.

**A restart re-reads DM history but does not re-judge it.** The window is
rebuilt by replaying every live `dm_message` row on boot, and verdicts on
those replayed rows are deliberately dropped: storage hands them back in
entity-id order rather than send order, so acting partway through a thread's
history would be acting on a half-read thread. Enforcement resumes once the
replay is done. A burst committed in the seconds before a restart therefore
escapes retroactive tombstoning — but it still counts, so the sender's next
message pays for it.

**What that public row discloses, stated plainly.** `modaction` is
world-readable by design, so the row tells the community that this member
tripped the DM rate limit, and carries the counts. It never names a recipient
and never contains message text. That trade is deliberate: without a public
row an automated tombstone would be invisible to the moderators who are
supposed to decide whether it warrants a ban — and since moderators cannot
read DMs, rows like this one and user reports are the only signal they get.

`ModAction.actor` is `None` on these rows: nothing human acted. The mod-log
panel renders that as "Automatic", which is deliberately distinct from the
"Unknown" it shows when an actor exists but cannot be named.

## Not in the DM lane, on purpose

- **The x-ray inspector refuses deleted DMs outright** (`xray/inspector.rs`),
  with no moderator escape hatch — unlike the room-message carve-out, which
  has one. DM history must not be one click more readable than room history
  (community#68 item 4).
- **DM read cursors are private even from the correspondent.** `dmreadstate`
  is scoped `user = $jwt.sub`, not to the participant pair: a read cursor is a
  read receipt, and shipping read receipts is a product decision nobody made.
- **Mentions inside DM text notify nobody.** A third party named in a private
  thread cannot read it, so telling them it exists would leak the fact of the
  conversation. `server/src/workers/dm_notify.rs` is a separate worker on a
  separate query precisely so this cannot be "fixed" by accident.
