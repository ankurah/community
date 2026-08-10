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
 * looks like. The server seeds rooms at boot (`ensure_default_rooms`), so the
 * room rail always has real content to render — but nothing seeds a MESSAGE,
 * and nothing in a browser can: a guest holds no `post` privilege and there is
 * no member session to write one with. The one server-side path that writes a
 * message without a member is `POST /hooks/ci`, which needs `CI_HOOK_SECRET`
 * configured for the e2e server and a copy of that secret in the spec — more
 * machinery, and a secret in the tree, than rung 1 is worth. So the timeline
 * is asserted for its shape and its refusals, never for its contents, which
 * also keeps these specs honest on a developer's machine: the sled node opens
 * `~/.community`, so locally it carries whatever a dev session left there
 * while on a CI runner it is empty.
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

/** The status `POST /auth/guest` answered the boot with. */
let guestMintStatus: number | undefined;

test.describe.configure({ mode: 'serial' });

test.describe('Guest flows (#79)', () => {
  test.beforeAll(async ({ browser }) => {
    context = await browser.newContext();

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
    // The boot resolves a session, connects the node, and waits for the
    // server's policy before it mounts anything at all, so the first thing to
    // wait for is the app itself. Generously bounded: on a cold CI runner this
    // covers a websocket handshake and one policy row over it.
    await expect(page.locator('.container')).toBeVisible({ timeout: 30_000 });
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
      await expect(page.locator('.roomItem', { hasText: `#${name}` })).toHaveCount(1, { timeout: 20_000 });
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
    // THE ROSTER IS THE ONE TO READ TWICE. `user` is signed-in-only in
    // `policy.json`, so a guest's members query would be refused at the
    // collection gate — and the header answers that by never offering the
    // button, which is why there is no panel to open and nothing to assert
    // inside one.
    await expect(page.locator('.membersButton')).toHaveCount(0);
    await expect(page.locator('.memberRow')).toHaveCount(0);

    // The rest of what a guest has no account for.
    await expect(page.locator('.modLogButton')).toHaveCount(0);
    await expect(page.locator('.notificationButton')).toHaveCount(0);
    await expect(page.locator('.xrayButton')).toHaveCount(0);
    await expect(page.locator('.accountSettingsButton')).toHaveCount(0);
    await expect(page.locator('.userInfo')).toHaveCount(0);
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

    // The press wrote nothing: the box still holds no text, and Send is still
    // refused.
    await expect(page.locator('.input[placeholder="Type a message..."]')).toHaveValue('');
    await expect(page.locator('.sendButton')).toBeDisabled();
  });

  test('the app is still there, and still read-only, after a sign-in that goes nowhere', async () => {
    // The hand-over above went nowhere (the route answered it), which is the
    // situation a visitor is in whenever they start a sign-in and do not
    // finish one. The app must still be the app.
    await expect(page.locator('.container')).toBeVisible();
    await expect(page.locator('.signIn')).toHaveCount(0);

    // Reading goes on: another room opens, and the address bar follows.
    const ci = page.locator('.roomItem', { hasText: '#ci' });
    await expect(ci).toHaveCount(1);
    await ci.click();
    await expect(page.locator('.roomItem.selected .roomLabel')).toHaveText('ci');
    await expect(page.locator('.messagesContainer')).toBeVisible();

    // And writing is still refused, in the room they moved to.
    const composer = page.locator('.input[placeholder="Type a message..."]');
    await expect(composer).toHaveAttribute('readonly', '');
    await expect(page.locator('.sendButton')).toBeDisabled();
  });
});
