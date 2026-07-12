# X-ray mode

X-ray mode is Community's showpiece: a built-in lens that lets you **watch the
live sync engine work**. You never need it to chat — it's here to show you *how*
the chat works, in real time, on real data.

## Why this app can show its own machinery

Community is **local-first**. Your browser isn't just a window onto a server —
it runs a real Ankurah node with its own local database and its own record of
every change, syncing quietly with the server and with everyone else. Because
all of that is happening *right there in your browser*, the app can turn a lens
on itself and show you the very events, connections, and queries keeping your
screen up to date. X-ray isn't a simulation; it's the actual machinery.

## Turning it on

The **magnifier** button in the top bar is a simple on/off switch — press it
once to turn x-ray on, and again to turn it fully off. **Alt + X** does the same
from anywhere.

Turning x-ray on opens the system panel and adds a marker to every message;
turning it off clears both. Community remembers your choice, so x-ray stays on
across reloads until you switch it off, and a link ending in `?xray=1` opens
straight into it — handy for showing someone.

## The three layers

X-ray works at three levels of zoom — from a single message, to one message's
full history, to the whole node.

### 1. A marker on every message

With x-ray on, each message gains a **small marker**. Hover it to see that
message's event id; click it to open the inspector (below). The marker turns
**amber** when a message has concurrent edits that haven't merged yet — a
glimpse of the sync engine reconciling changes made in two places at once.

### 2. The entity inspector

Clicking a message's marker (or **Inspect** in its menu) opens a drawer showing
that message's **complete history as a graph**. Every change is a node; lines
connect each change to the one it followed. Select a node to see what it did —
which fields it wrote, how big it was, and whether it was already stored **on
your device** or had to be **fetched from a peer**. You can inspect rooms and
people the same way.

(One rule: the history of a *deleted* message can only be inspected by
moderators.)

### 3. The system panel

The side panel is the widest view — the state of your local node, in four live
cards:

- **This node** — your browser's node: its identity, whether it keeps data
  permanently (the server) or is a temporary client (your browser is), whether
  it's fully caught up, and how much of the conversation it has cached locally.
- **Connection & peers** — the live connection state, a running log of every
  connect and disconnect, and the server it's talking to.
- **Live queries** — the live questions your client is currently asking of the
  data (for example, "the messages in this room"), each updating as answers
  arrive.
- **Live event feed** — a running list of changes as they land, newest first.
  Click any row to open that entity in the inspector.

The panel has its own **×** that tucks it away while leaving the per-message
markers in place — so you can keep the ambient view without the side panel. To
switch x-ray off completely, press the lens button (or **Alt + X**) again.

The whole app stays fully usable while the panel is open — which makes for a fun
demonstration: watch the **Connection & peers** card narrate a reconnect after a
network blip, all while the conversation keeps scrolling along beside it.
