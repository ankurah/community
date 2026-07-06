import { test, expect } from '@playwright/test';

// Two isolated browser contexts = two distinct ankurah nodes/users (each with its
// own localStorage + IndexedDB). Same-profile tabs would share the stored user id,
// so isolated contexts are what make this a genuine MULTI-user scenario.
//
// This exercises real-time sync through the server both directions, and a
// concurrent-send case that specifically stresses PR #201 (concurrent updates /
// causal comparison in the event DAG).
// SKIPPED (OIDC, issue #1): requires idp.to sign-in — see chat.spec.ts. Pending an
// OIDC-aware e2e rewrite.
test.skip('multi-user: rooms and messages sync between two users', async ({ browser }) => {
  test.setTimeout(90_000);

  const ctxA = await browser.newContext();
  const ctxB = await browser.newContext();
  const A = await ctxA.newPage();
  const B = await ctxB.newPage();

  await A.goto('/');
  await B.goto('/');

  // Both nodes connect to the server.
  await expect(A.locator('.connectionStatus')).toContainText('Connected', { timeout: 30_000 });
  await expect(B.locator('.connectionStatus')).toContainText('Connected', { timeout: 30_000 });

  // Each context bootstraps its own user (wait for the display name to resolve).
  await expect(A.locator('.userName')).not.toContainText('Loading', { timeout: 15_000 });
  await expect(B.locator('.userName')).not.toContainText('Loading', { timeout: 15_000 });
  const nameA = (await A.locator('.userName').innerText()).trim();
  const nameB = (await B.locator('.userName').innerText()).trim();
  console.log(`user A = "${nameA}"  |  user B = "${nameB}"`);

  // --- Room sync (A creates -> B sees) ------------------------------------
  const room = `MultiUser-${Date.now()}`;
  await A.click('.createRoomButton');
  const roomInputA = A.locator('.createRoomInput input');
  await roomInputA.fill(room);
  await roomInputA.press('Enter');
  await expect(A.locator('.roomItem', { hasText: room })).toHaveClass(/selected/, { timeout: 5_000 });

  const roomItemB = B.locator('.roomItem', { hasText: room });
  await expect(roomItemB).toBeVisible({ timeout: 15_000 }); // room entity synced A -> B
  await roomItemB.click();
  await expect(roomItemB).toHaveClass(/selected/);

  const inputA = A.locator('.input[placeholder="Type a message..."]');
  const inputB = B.locator('.input[placeholder="Type a message..."]');

  // --- Message sync A -> B -------------------------------------------------
  const msgA = `From A ${Date.now()}`;
  await expect(inputA).toBeEnabled({ timeout: 5_000 });
  await inputA.fill(msgA);
  await A.click('.button:has-text("Send")');
  await expect(A.locator('.messagesContainer')).toContainText(msgA, { timeout: 5_000 });
  await expect(B.locator('.messagesContainer')).toContainText(msgA, { timeout: 15_000 });

  // --- Message sync B -> A -------------------------------------------------
  const msgB = `From B ${Date.now()}`;
  await expect(inputB).toBeEnabled({ timeout: 5_000 });
  await inputB.fill(msgB);
  await B.click('.button:has-text("Send")');
  await expect(B.locator('.messagesContainer')).toContainText(msgB, { timeout: 5_000 });
  await expect(A.locator('.messagesContainer')).toContainText(msgB, { timeout: 15_000 });

  // --- Concurrent sends (PR #201 causal-comparison stress) -----------------
  // Both users send at (nearly) the same instant; both messages must converge
  // for both users.
  const cA = `ConcurrentA ${Date.now()}`;
  const cB = `ConcurrentB ${Date.now()}`;
  await inputA.fill(cA);
  await inputB.fill(cB);
  await Promise.all([
    A.click('.button:has-text("Send")'),
    B.click('.button:has-text("Send")'),
  ]);
  for (const p of [A, B]) {
    await expect(p.locator('.messagesContainer')).toContainText(cA, { timeout: 15_000 });
    await expect(p.locator('.messagesContainer')).toContainText(cB, { timeout: 15_000 });
  }

  // Evidence.
  await A.screenshot({ path: 'test-results/multiuser-A.png', fullPage: true });
  await B.screenshot({ path: 'test-results/multiuser-B.png', fullPage: true });

  await ctxA.close();
  await ctxB.close();
});
