import { test, expect } from '@playwright/test';

test.describe('Chat Application', () => {
  test.beforeEach(async ({ page }) => {
    // Clear localStorage to get a fresh user each test
    await page.goto('/');
    await page.evaluate(() => localStorage.clear());
    await page.reload();
  });

  test('connects and displays user', async ({ page }) => {
    // Wait for WebSocket connection
    await expect(page.locator('.connectionStatus')).toContainText('Connected', { timeout: 30000 });

    // User should be auto-created with a display name
    await expect(page.locator('.userName')).not.toContainText('Loading');
    await expect(page.locator('.userName')).toBeVisible();
  });

  test('can create a room, select it, send message, and verify it appears', async ({ page }) => {
    // Wait for connection
    await expect(page.locator('.connectionStatus')).toContainText('Connected', { timeout: 30000 });

    // Generate unique room name for this test
    const roomName = `TestRoom-${Date.now()}`;

    // Click the "+" button to create a new room
    await page.click('.createRoomButton');

    // Type room name and submit
    const roomInput = page.locator('.createRoomInput input');
    await expect(roomInput).toBeVisible();
    await roomInput.fill(roomName);
    await roomInput.press('Enter');

    // Verify room appears in sidebar and is selected
    const roomItem = page.locator('.roomItem', { hasText: roomName });
    await expect(roomItem).toBeVisible({ timeout: 5000 });
    await expect(roomItem).toHaveClass(/selected/);

    // Verify chat area is ready (no "Select a room" message)
    await expect(page.locator('.emptyState')).not.toContainText('Select a room');

    // Send a test message
    const testMessage = `Hello from E2E! ${Date.now()}`;
    const messageInput = page.locator('.input[placeholder="Type a message..."]');
    await expect(messageInput).toBeEnabled({ timeout: 5000 });
    await messageInput.fill(testMessage);

    // Click send button
    await page.click('.button:has-text("Send")');

    // Verify message appears in the messages container
    // This tests that Ref<User> and Ref<Room> are correctly processed
    await expect(page.locator('.messagesContainer')).toContainText(testMessage, { timeout: 5000 });
  });

  test('message with Ref<User> is correctly stored and retrievable', async ({ page }) => {
    // This test validates that Ref<User> is properly serialized when creating a message
    // and can be resolved back when displaying messages

    // Wait for connection
    await expect(page.locator('.connectionStatus')).toContainText('Connected', { timeout: 30000 });

    // Create a room
    const roomName = `RefTest-${Date.now()}`;
    await page.click('.createRoomButton');
    const roomInput = page.locator('.createRoomInput input');
    await roomInput.fill(roomName);
    await roomInput.press('Enter');

    // Wait for room to be selected
    await expect(page.locator('.roomItem.selected', { hasText: roomName })).toBeVisible();

    // Send a message - this exercises Ref<User> and Ref<Room> serialization
    const messageInput = page.locator('.input[placeholder="Type a message..."]');
    await expect(messageInput).toBeEnabled();
    await messageInput.fill('Testing Ref serialization');
    await page.click('.button:has-text("Send")');

    // Verify the message appears (proves the Ref<> fields were correctly processed)
    const messageBubble = page.locator('.messageBubble').first();
    await expect(messageBubble).toBeVisible({ timeout: 5000 });
    await expect(messageBubble).toContainText('Testing Ref serialization');

    // Verify the message has a valid ID (proves it was saved to the database)
    const msgId = await messageBubble.getAttribute('data-msg-id');
    expect(msgId).toBeTruthy();
    expect(msgId!.length).toBeGreaterThan(10); // Base64 EntityId is ~22 chars
  });
});
