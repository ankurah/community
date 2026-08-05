import { test, expect } from '@playwright/test';

// SKIPPED (OIDC, issue #1): every spec in this repo that needs a signed-in
// session is skipped, because the app requires idp.to sign-in and idp.to is
// passkey-first — Playwright cannot drive it headlessly. These specs are
// written against the real selectors and are meant to run unchanged once the
// suite grows an auth seam (dev-mint endpoint or a WebAuthn virtual
// authenticator; roadmap item 1.10).
//
// DMs also ship dark behind community#68: the upstream event-read fix
// (ankurah#438) must be released and pinned before DMs see real use. Nothing
// here depends on that — these specs exercise the client's own convergence
// behaviour — but a reviewer un-skipping them should know the feature is not
// yet meant to carry traffic.
test.describe.skip('Direct messages (#30)', () => {
  test('start a DM from a member card and send a message', async ({ browser }) => {
    test.setTimeout(90_000);

    // Two isolated contexts = two distinct users, which is what a DM needs.
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const A = await ctxA.newPage();
    const B = await ctxB.newPage();
    await A.goto('/');
    await B.goto('/');
    await expect(A.locator('.connectionStatus')).toContainText('Connected', { timeout: 30_000 });
    await expect(B.locator('.connectionStatus')).toContainText('Connected', { timeout: 30_000 });
    await expect(B.locator('.userName')).not.toContainText('Loading', { timeout: 15_000 });
    const nameB = (await B.locator('.userName').innerText()).trim();

    // A opens B's member card and starts the conversation. The member sidebar
    // (#57) is the only entry point — the DM sidebar section lists what
    // already exists, it does not create.
    await A.click('.membersButton');
    await A.locator('.memberRow', { hasText: nameB }).click();
    await A.click('.userDetailMessageBtn');

    // The thread view opens, titled with the other participant.
    await expect(A.locator('.dmThreadWith')).toContainText(nameB, { timeout: 15_000 });

    const text = `DM from A ${Date.now()}`;
    const input = A.locator('.input[placeholder="Type a message..."]');
    await expect(input).toBeEnabled({ timeout: 5_000 });
    await input.fill(text);
    await A.click('.button:has-text("Send")');
    await expect(A.locator('.messagesContainer')).toContainText(text, { timeout: 5_000 });

    // B sees the conversation appear in their sidebar with an unread badge,
    // and reads it. A thread with no messages would NOT appear — the first
    // message is what surfaces the conversation for the recipient.
    const threadRowB = B.locator('.dmItem').first();
    await expect(threadRowB).toBeVisible({ timeout: 20_000 });
    await expect(threadRowB.locator('.unreadBadge')).toBeVisible({ timeout: 20_000 });
    await threadRowB.click();
    await expect(B.locator('.messagesContainer')).toContainText(text, { timeout: 10_000 });
    // Reading the live tail advances the cursor, so the badge clears.
    await expect(threadRowB.locator('.unreadBadge')).toHaveCount(0, { timeout: 10_000 });

    await ctxA.close();
    await ctxB.close();
  });

  test('double-tab first-DM race resolves to one conversation', async ({ browser }) => {
    test.setTimeout(90_000);

    // ONE user in TWO tabs of the SAME context — same localStorage, same
    // identity — which is the race the portfolio's fixed-partner panel makes
    // likely: a visitor clicks "DM Daniel" in two tabs before either thread
    // has synced. Both tabs find no thread and both create one.
    const ctxA = await browser.newContext();
    const tab1 = await ctxA.newPage();
    const tab2 = await ctxA.newPage();
    const ctxB = await browser.newContext();
    const B = await ctxB.newPage();

    await Promise.all([tab1.goto('/'), tab2.goto('/'), B.goto('/')]);
    for (const p of [tab1, tab2, B]) {
      await expect(p.locator('.connectionStatus')).toContainText('Connected', { timeout: 30_000 });
    }
    await expect(B.locator('.userName')).not.toContainText('Loading', { timeout: 15_000 });
    const nameB = (await B.locator('.userName').innerText()).trim();

    // Both tabs press "Message" on the same person at the same moment.
    const startDm = async (page: typeof tab1) => {
      await page.click('.membersButton');
      await page.locator('.memberRow', { hasText: nameB }).click();
      await page.click('.userDetailMessageBtn');
    };
    await Promise.all([startDm(tab1), startDm(tab2)]);

    for (const p of [tab1, tab2]) {
      await expect(p.locator('.dmThreadWith')).toContainText(nameB, { timeout: 15_000 });
    }

    // THE ASSERTION: one conversation, not two. The sidebar collapses
    // duplicates by participant pair, so a correspondent appears exactly once
    // however many rows the race produced.
    await expect(tab1.locator('.dmItem')).toHaveCount(1, { timeout: 20_000 });
    await expect(tab2.locator('.dmItem')).toHaveCount(1, { timeout: 20_000 });
    await expect(B.locator('.dmItem')).toHaveCount(1, { timeout: 20_000 });

    // Both tabs show both messages, whichever row each send landed in. That is
    // NOT convergence — a selection names a correspondent, never a row, so
    // there is nothing to converge — it is the read side: a conversation view
    // reads every row its participant pair has, so words written into the
    // losing twin during the race stay part of the conversation.
    const m1 = `from tab one ${Date.now()}`;
    const m2 = `from tab two ${Date.now()}`;
    await tab1.locator('.input[placeholder="Type a message..."]').fill(m1);
    await tab1.click('.button:has-text("Send")');
    await tab2.locator('.input[placeholder="Type a message..."]').fill(m2);
    await tab2.click('.button:has-text("Send")');

    for (const p of [tab1, tab2]) {
      await expect(p.locator('.messagesContainer')).toContainText(m1, { timeout: 15_000 });
      await expect(p.locator('.messagesContainer')).toContainText(m2, { timeout: 15_000 });
    }
    // The recipient sees one conversation carrying both messages — the proof
    // that the conversation did not fork.
    await B.locator('.dmItem').first().click();
    await expect(B.locator('.messagesContainer')).toContainText(m1, { timeout: 15_000 });
    await expect(B.locator('.messagesContainer')).toContainText(m2, { timeout: 15_000 });

    // A second exchange, after the race window has closed.
    //
    // WHAT THIS PINS: cross-tab delivery once things have settled, and that the
    // recipient still sees exactly one conversation rather than two.
    //
    // WHAT IT DOES NOT PIN, said plainly so nobody reads more into it: that the
    // two tabs write into the SAME thread row. Every assertion here is
    // satisfied by cross-pair reads — a conversation view reads all of the
    // pair's rows — so both tabs could go on writing into different twins
    // forever and this would still pass. Nothing on screen carries the thread a
    // message belongs to, and nothing observes which of a pair's rows is
    // canonical.
    //
    // WHAT WOULD PIN IT, for whoever rewrites these against OIDC (issue #1):
    // count the pair's `dmthread` rows and assert the SETTLED messages — the
    // ones sent after both tabs see each other — hang off the lowest id
    // (messages sent during the race window may legitimately remain on a twin;
    // resolution is per send). Either query the store from the page, or put the
    // thread ref on the row's DOM (a `data-thread-id` beside `data-msg-id`)
    // so a locator can see it. Until then, canonical write routing is pinned by
    // the server-side convergence tests and by reading the code, not by this.
    const settled1 = `settled tab one ${Date.now()}`;
    const settled2 = `settled tab two ${Date.now()}`;
    await tab1.locator('.input[placeholder="Type a message..."]').fill(settled1);
    await tab1.click('.button:has-text("Send")');
    await expect(tab2.locator('.messagesContainer')).toContainText(settled1, { timeout: 15_000 });
    await tab2.locator('.input[placeholder="Type a message..."]').fill(settled2);
    await tab2.click('.button:has-text("Send")');
    await expect(tab1.locator('.messagesContainer')).toContainText(settled2, { timeout: 15_000 });
    await expect(B.locator('.dmItem')).toHaveCount(1, { timeout: 20_000 });

    await ctxA.close();
    await ctxB.close();
  });

  test('a third party never sees a thread they are not in', async ({ browser }) => {
    test.setTimeout(90_000);

    // The adversarial leg, from the UI side. The authoritative version of this
    // claim is server/tests/dm_policy_live_tests.rs, which asserts on fetch,
    // live delivery and get-by-id under the real policy; this only checks that
    // nothing leaks into the rendered surface.
    const ctxA = await browser.newContext();
    const ctxB = await browser.newContext();
    const ctxC = await browser.newContext();
    const [A, B, C] = [await ctxA.newPage(), await ctxB.newPage(), await ctxC.newPage()];
    await Promise.all([A.goto('/'), B.goto('/'), C.goto('/')]);
    for (const p of [A, B, C]) {
      await expect(p.locator('.connectionStatus')).toContainText('Connected', { timeout: 30_000 });
    }
    await expect(B.locator('.userName')).not.toContainText('Loading', { timeout: 15_000 });
    const nameB = (await B.locator('.userName').innerText()).trim();

    await A.click('.membersButton');
    await A.locator('.memberRow', { hasText: nameB }).click();
    await A.click('.userDetailMessageBtn');
    const secret = `not for C ${Date.now()}`;
    await A.locator('.input[placeholder="Type a message..."]').fill(secret);
    await A.click('.button:has-text("Send")');

    // B receives it...
    await expect(B.locator('.dmItem')).toHaveCount(1, { timeout: 20_000 });
    // ...and C, signed in and connected, has no conversation at all.
    await expect(C.locator('.dmItem')).toHaveCount(0);
    await expect(C.locator('body')).not.toContainText(secret);

    await ctxA.close();
    await ctxB.close();
    await ctxC.close();
  });
});
