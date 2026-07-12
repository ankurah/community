# Messages

## Writing a message

Type in the box at the bottom and press **Enter** to send. That's it — your
message appears for everyone instantly.

The box is available whenever you're connected. If the connection drops, it's
disabled until you're back online, so nothing is lost to a bad moment of
network.

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

The compose box is a single line, so the formatting you can type is the inline
kind above — **bold**, *italic*, `` `inline code` ``, and links. (Multi-line
**code blocks** fenced in triple backticks do render when a message contains
them, but you can't type them in the box yet — richer composing is on the way.)

If a message has no special characters, it's shown exactly as typed — so a
message that just mentions `*` or `#` in passing stays plain.

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

Only the author can edit a message; there's no way for someone else to change
your words.

## Deleting a message

From the same **⋯** / right-click menu, choose **Delete** to remove your own
message. Deleted messages don't vanish from the conversation — they're replaced
in place by a muted note reading **"Removed by the author"**. The text itself is
gone, but the gap in the timeline stays honest about the fact that something was
there. A removed message can't be edited or reacted to.

Moderators can remove anyone's message; see [Moderation](moderation.md) for how
that looks and how it's kept transparent.
