# Messages

## Writing a message

Type in the box at the bottom and press **Enter** to send. That's it — your
message appears for everyone instantly. **Shift+Enter** adds a line break, so
multi-line messages (and code blocks) are just typing.

The box is available whenever you're connected. If the connection drops, it's
disabled until you're back online, so nothing is lost to a bad moment of
network.

## Mentions and emoji

Type **@** and keep typing a name — a picker appears; **Enter** or **Tab**
inserts the mention. The box shows a plain **@Name** while you compose (no
codes, no clutter); when you send, it becomes a real mention — highlighted in
the message and delivered to that person's notification bell.

Emoji work the same way with colons: type **:** and two or more letters for a
picker (`:tada:` 🎉, `:thumbsup:` 👍, `:fire:` 🔥 …), or type the full
`:shortcode:` and it converts on the closing colon. Messages store the emoji
itself, so they look right everywhere.

## Formatting with Markdown

Messages support a familiar subset of **Markdown**, so you can add light
formatting:

| You type            | You get                          |
| ------------------- | -------------------------------- |
| `**bold**`          | **bold**                         |
| `*italic*`          | *italic*                         |
| `` `inline code` `` | `inline code`                    |
| `[a link](https://ankurah.org)` | a clickable link     |

A few sensible rules keep things tidy and safe:

- **Links** open in a new tab. Only ordinary web links (`http` and `https`)
  become clickable; anything else is shown as plain text.
- **Headings, quotes, and lists** render simply and never swallow your text.
- **Images aren't loaded** — if a message contains an image link, you see its
  description text instead. This keeps the chat fast and private.
- **No raw HTML.** Any HTML in a message is ignored rather than rendered, which
  keeps everyone safe from dodgy markup.

Multi-line **code blocks** work too: type ` ``` ` on its own line
(**Shift+Enter** for the line breaks), paste or write your code, close with
` ``` `, and send.

If a message has no special characters, it's shown exactly as typed — so a
message that just mentions `*` or `#` in passing stays plain.

## Replying

Choose **Reply** from a message's **⋯** / right-click menu. A small
"Replying to…" chip appears above the compose box — your draft, if you had
one, stays put — and the **×** on the chip (or **Esc**) cancels. Send, and
your message carries a compact preview of the original above your words.
Clicking that preview jumps back to the original message when it's on screen,
with a brief highlight so your eye lands on it. If the original was removed,
the preview says so honestly.

## Editing a message

You can edit **your own** messages. Two ways:

- Hover the message and click the **⋯** button (or right-click the message),
  then choose **Edit message**.
- Or, with the compose box empty, press **Cmd/Ctrl + ↑** to jump straight into
  editing your most recent message. Keep pressing to step further back;
  **Cmd/Ctrl + ↓** steps forward again.

Make your change and press **Enter** to save, or **Esc** to cancel. An edited
message carries a small **(edited)** marker — hover it to see when it was last
changed. Saving without actually changing anything doesn't mark the message.

Only the author can edit a message — unless the author has deliberately opened
it up with **"Allow others to edit"** (such messages carry a small **co-edit**
badge). Nobody else can change your words otherwise.

## Deleting a message

From the same **⋯** / right-click menu, choose **Delete** to remove your own
message. Deleted messages don't vanish from the conversation — they're replaced
in place by a muted note reading **"Removed by the author"**. The text itself is
gone, but the gap in the timeline stays honest about the fact that something was
there. A removed message can't be edited or reacted to.

Moderators can remove anyone's message; see [Moderation](moderation.md) for how
that looks and how it's kept transparent.
