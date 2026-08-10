import { test, expect, type BrowserContext, type Page } from '@playwright/test';

/**
 * Guest flows — the read-only session a visitor gets without signing in (#79).
 *
 * These are the only LIVE specs in this suite. Every other spec here is
 * skipped, because a member session comes from idp.to and idp.to is
 * passkey-first, which Playwright cannot drive. A guest session comes from our
 * own server — `POST /auth/guest`, no IdP round trip — so the guest surface is
 * the part of the app a browser can actually reach in a test.
 *
 * WHAT IS ASSERTED, AND WHAT IS DELIBERATELY NOT. What a guest may READ is
 * settled by `policy.json` and proved against a real node in
 * `server/tests/guest_policy_live_tests.rs`; nothing here re-proves it. What
 * these specs pin is the browser half: that a fresh visit becomes a guest
 * session and mounts the app rather than the sign-in card, that the surfaces a
 * guest has no account for are ABSENT rather than present-and-refusing, that
 * the message box refuses a caret, and that reaching for a write raises a
 * sign-in instead of performing one.
 *
 * NO SEEDED CONTENT, and therefore no assertion about what a message row
 * looks like. The server seeds rooms at boot (`ensure_default_rooms`, plus
 * `ci_hook::seed`'s `#ci` room), so the room rail always has real content to
 * render — but nothing seeds a MESSAGE, and nothing in a browser can: a guest
 * holds no `post` privilege and there is no member session to write one with.
 * The one server-side path that writes a message without a member is
 * `POST /hooks/ci`, which needs `CI_HOOK_SECRET` configured for the e2e
 * server and a copy of that secret in the spec — more machinery, and a secret
 * in the tree, than rung 1 is worth. So the timeline is asserted for its
 * shape and its refusals, never for its contents. The harness boots the node
 * on a per-run scratch directory (`COMMUNITY_DATA_DIR`), so the specs run
 * against an empty store everywhere — except when dev.sh's exported ports
 * attach them to an already-running dev node, which is why they stay
 * content-agnostic rather than asserting emptiness.
 *
 * THE MINT BUDGET IS WHY THERE IS ONE PAGE. `POST /auth/guest` admits ten
 * mints per client address per minute (`server/src/guest.rs`), and every load
 * of the app mints one — nothing about a guest session is stored, so a reload
 * is a fresh mint. A spec file that opened a fresh page per test would spend
 * that budget on its own retries and start landing on the sign-in card for a
 * reason that has nothing to do with what it was testing. One session is
 * opened here and every test reads it, in order.
 */

/** Where the boot's authorization request would have gone, if it went. */
const IDP_AUTHORIZE = 'https://id.idp.to/oidc/authorize';

let context: BrowserContext;
let page: Page;

/**
 * Every request the page made to idp.to, in order. It should be empty for the
 * whole of a guest's visit until they reach for something a member does — see
 * the sign-in test, which is the only one that expects an entry.
 */
let idpRequests: string[] = [];

/**
 * Every request the page addressed to any host that is neither this run's own
 * localhost nor idp.to. There is no such host in the app today, and the last
 * test asserts there still isn't — a future asset host would otherwise reach
 * the network from CI unremarked.
 */
let strayRequests: string[] = [];

/** The status `POST /auth/guest` answered the boot with. */
let guestMintStatus: number | undefined;

/**
 * The rail row for a room, by its exact name. Matching the row's text would
 * also find a room called `general-2`, and a developer's node carries whatever
 * rooms their own dev session created — so the name is anchored against the
 * label, which holds it bare (the `#` is a separate span). For a MEMBER
 * session this locator would be too loose: DM rows are `.roomItem dmItem`
 * with the partner's name in the same `.roomLabel`, so a partner named
 * `general` would collide. Guests have no DM rows (asserted below), so it is
 * exact here.
 */
const roomRow = (name: string) =>
  page.locator('.roomItem').filter({ has: page.locator('.roomLabel', { hasText: new RegExp(`^${name}$`) }) });

test.describe.configure({ mode: 'serial' });

test.describe('Guest flows (#79)', () => {
  test.beforeAll(async ({ browser }) => {
    // The suite's one page load pays the wasm compile, and on a shared
    // two-vCPU CI runner that is the slow part: the ~20MB module (trunk
    // serve skips the wasm-opt pass the deploy pipeline runs) compiles
    // while the server, trunk, and the runner share the same cores. Proven
    // from an uploaded trace — every asset landed within 200ms and the page
    // then sat out the whole default window without the module finishing.
    // The bound is for the runner's worst day; local runs never come near it.
    test.setTimeout(240_000);

    context = await browser.newContext();

    // The outer fence: any host that is not this run's own server. Registered
    // FIRST so the idp.to route below (registered later, matched first) takes
    // idp traffic out of it; what remains here should be nothing at all, and
    // the last test asserts exactly that. Fulfilled rather than aborted so an
    // accidental navigation strands the page in place instead of killing it.
    await context.route(
      (url) => url.hostname !== 'localhost' && url.hostname !== '127.0.0.1',
      async (route) => {
        strayRequests.push(route.request().url());
        await route.fulfill({ status: 204, body: '' });
      },
    );

    // idp.to IS NOT MOCKED ANYWHERE, and this is what keeps it out of the run.
    // Every request to any idp.to host is answered here, by Playwright, with a
    // 204 that never leaves the machine — so the suite has no external
    // dependency to be flaky about, and a spec that accidentally started a
    // real sign-in fails rather than firing a live authorization request at
    // somebody else's service. 204 rather than an abort because the browser
    // treats it as "stay where you are": a top-level navigation answered this
    // way leaves the app on screen, which is what the sign-in test then goes
    // on to assert.
    await context.route(
      (url) => url.hostname === 'idp.to' || url.hostname.endsWith('.idp.to'),
      async (route) => {
        idpRequests.push(route.request().url());
        await route.fulfill({ status: 204, body: '' });
      },
    );

    page = await context.newPage();
    page.on('response', (response) => {
      if (new URL(response.url()).pathname === '/auth/guest') {
        guestMintStatus = response.status();
      }
    });

    await page.goto('/');
    // The boot compiles the module, resolves a session, connects the node,
    // and waits for the server's policy before it mounts anything at all, so
    // the first thing to wait for is the app itself — bounded by the wasm
    // compile above, not by any of our own machinery.
    await expect(page.locator('.container')).toBeVisible({ timeout: 180_000 });
  });

  test.afterAll(async () => {
    await context?.close();
  });

  test('a fresh visit becomes a guest session, minted by our own server', async () => {
    // The mint is ours, and it is the whole of how this visitor got a session:
    // one POST to our server, and not a single request to idp.to.
    expect(guestMintStatus).toBe(200);
    expect(idpRequests).toEqual([]);

    // The card is what a visitor sees when the boot could not hand them
    // anything to read with. It is not what a fresh visit gets.
    await expect(page.locator('.signIn')).toHaveCount(0);
    await expect(page.locator('.connectionStatus')).toContainText('Connected', { timeout: 30_000 });
  });

  test('the seeded rooms render, with general selected', async () => {
    // `ensure_default_rooms` seeds these at every boot, so they are here
    // whatever else the node carries. Membership rather than an exact list:
    // a developer's node has whatever rooms their own dev session created.
    for (const name of ['general', 'support', 'announcements', 'introductions']) {
      await expect(roomRow(name)).toHaveCount(1, { timeout: 20_000 });
    }

    // Nobody picked a room, so the rail picks `general` — and the address bar
    // follows the choice, which is what makes a room linkable.
    await expect(page.locator('.roomItem.selected .roomLabel')).toHaveText('general');
    expect(page.url()).toContain('?room=');
  });

  test('the room timeline is mounted and refuses a caret', async () => {
    await expect(page.locator('.messagesContainer')).toBeVisible();

    // The message box is present and REFUSES rather than being disabled: an
    // anonymous reader is meant to press it and be offered a sign-in, so it
    // has to be pressable. What stops a draft is `readonly`, and `tabindex`
    // keeps a keyboard out of a box that would take nothing.
    const composer = page.locator('.input[placeholder="Type a message..."]');
    await expect(composer).toBeVisible();
    await expect(composer).toHaveAttribute('readonly', '');
    await expect(composer).toHaveAttribute('tabindex', '-1');
    await expect(composer).toBeEnabled();
    await expect(page.locator('.sendButton')).toBeDisabled();

    // Creating a room is a write too, and the rail simply does not offer it.
    await expect(page.locator('.createRoomButton')).toHaveCount(0);
  });

  test('the member surfaces are absent rather than present-and-refusing', async () => {
    // THE ROSTER IS THE ONE TO READ TWICE. LISTING members is signed-in-only
    // (`user`'s `read` scope in `policy.json`), while resolving one author BY
    // REF is open to the view tier (`retrieve: view`) — which is why message
    // rows do carry author names for a guest, and why what these lines assert
    // is only that no roster surface is offered: the header never renders the
    // button, so there is no panel to open and nothing to assert inside one.
    await expect(page.locator('.membersButton')).toHaveCount(0);
    await expect(page.locator('.memberRow')).toHaveCount(0);

    // The rest of what a guest has no account for.
    await expect(page.locator('.modLogButton')).toHaveCount(0);
    await expect(page.locator('.notificationButton')).toHaveCount(0);
    await expect(page.locator('.xrayButton')).toHaveCount(0);
    await expect(page.locator('.accountSettingsButton')).toHaveCount(0);
    await expect(page.locator('.userInfo')).toHaveCount(0);
    // The heading, not just the rows: `.dmItem` is empty for a guest even if
    // the section wrongly mounts (a guest has no conversations to list), so
    // the section header is what proves the surface itself is absent.
    await expect(page.locator('.dmSectionHeader')).toHaveCount(0);
    await expect(page.locator('.dmItem')).toHaveCount(0);

    // What stays is what is about the page rather than about who is reading
    // it — and, in the sign-out button's place, the way in.
    await expect(page.locator('.qrButton')).toHaveCount(1);
    await expect(page.locator('.signOutButton')).toHaveText('Sign in');
  });

  test('pressing the message box raises a sign-in rather than a draft', async () => {
    expect(idpRequests).toEqual([]);

    await page.locator('.input[placeholder="Type a message..."]').click();

    // WHAT A RAISED SIGN-IN LOOKS LIKE FROM A TEST ORIGIN, and why it is not
    // the in-page ceremony. `begin_framed_sign_in` frames idp.to's page only
    // on an origin idp.to has registered as an embedder, and the registry is
    // exact, port included: production, and `http://127.0.0.1:5173` for dev.
    // The harness serves the SPA on `http://localhost:<random port>`, which
    // matches neither, so the flow takes its other branch and hands the whole
    // tab to idp.to. That is the real behaviour at this origin — the framed
    // ceremony, its × and its Escape are simply not reachable from a spec
    // until the harness can be served from a registered origin.
    await expect.poll(() => idpRequests.length, { timeout: 10_000 }).toBe(1);
    const authorize = new URL(idpRequests[0]);
    expect(authorize.origin + authorize.pathname).toBe(IDP_AUTHORIZE);
    expect(authorize.searchParams.get('response_type')).toBe('code');
    // PKCE, and a redirect back to this origin: the press started the real
    // sign-in, not some other navigation that happened to go to idp.to.
    expect(authorize.searchParams.get('code_challenge_method')).toBe('S256');
    expect(authorize.searchParams.get('code_challenge')).toBeTruthy();
    expect(authorize.searchParams.get('redirect_uri')).toBe(`${new URL(page.url()).origin}/auth/callback`);

    // The press wrote nothing — and neither does typing. The click above
    // already put focus wherever the component leaves it, so keys sent now
    // are exactly what a visitor mashing the keyboard produces; `readonly` is
    // what eats them. (Typed here rather than in the caret test above: the
    // caret test must not click, because a click is the sign-in gesture and
    // its authorization request belongs to THIS test's count.)
    await page.keyboard.type('nope');
    await expect(page.locator('.input[placeholder="Type a message..."]')).toHaveValue('');
    await expect(page.locator('.sendButton')).toBeDisabled();
  });

  test('the app is still there, and still read-only, after a sign-in that goes nowhere', async () => {
    // The hand-over above went nowhere (the route answered it), which is the
    // situation a visitor is in whenever they start a sign-in and do not
    // finish one. The app must still be the app.
    await expect(page.locator('.container')).toBeVisible();
    await expect(page.locator('.signIn')).toHaveCount(0);

    // Reading goes on: another room opens, and the rail follows. `ci` is not
    // one of `ensure_default_rooms`' four — `ci_hook::seed` creates it
    // unconditionally at boot, secret configured or not, which is why it is
    // here to click on an otherwise-empty node.
    const ci = roomRow('ci');
    await expect(ci).toHaveCount(1);
    await ci.click();
    await expect(page.locator('.roomItem.selected .roomLabel')).toHaveText('ci');
    await expect(page.locator('.messagesContainer')).toBeVisible();

    // And writing is still refused, in the room they moved to.
    const composer = page.locator('.input[placeholder="Type a message..."]');
    await expect(composer).toHaveAttribute('readonly', '');
    await expect(page.locator('.sendButton')).toBeDisabled();

    // The whole visit's ledger, closed out: the one authorization request the
    // sign-in test claimed is still the only one — nothing later raised a
    // second, or trickled a delayed discovery fetch — and no request ever
    // addressed a host that is not ours or idp.to's.
    expect(idpRequests).toHaveLength(1);
    expect(strayRequests).toEqual([]);
  });
});
