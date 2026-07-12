# Moderation

Community is kept tidy by **moderators** (and admins, who can do the same).
Moderation here is deliberately **transparent** — nothing happens behind the
curtain.

## What moderators can do

- **Remove any message.** Using the same **⋯** / right-click menu you use on
  your own messages, a moderator can delete anyone's. They may add a short,
  public reason. The message becomes a muted note reading **"Removed by a
  moderator"** — the text is gone, but the fact of the removal stays visible.
- **Ban a member.** A moderator opens a member's detail sidebar (click their
  row in the members panel, their name on a profile card, or an @mention) and
  chooses **Ban member**, optionally with a reason.
- **Unban a member.** The same place lifts a ban.

Because roles come from the identity provider, a moderator's abilities are the
same everywhere they sign in.

## The public moderation log

The **gavel** button in the top bar opens the **moderation log** — and it's
open to *everyone*, not just moderators. Every moderator action lands here:
message removals, bans, and unbans, each showing who did it, what they did, any
reason they gave, and when.

This is the point of moderation in Community: **deleted messages are hidden,
never quietly erased.** If something was removed, anyone can see that it was,
and why.

## What being banned looks like

If you're banned, the chat is replaced by a full-screen notice telling you
you've been removed, along with the moderator's reason if they gave one. After a
few seconds you're signed out automatically — there's also a **Sign out now**
button if you'd rather not wait.

While a ban stands, you can't sign back in. If you think it's a mistake, the
notice suggests contacting a moderator.

## For the curious

The paragraphs above cover what you'll see. If you want the technical model —
exactly which layer enforces what, and how bans are recorded — the developer
documentation goes into detail in [`docs/moderation.md`](../moderation.md).
