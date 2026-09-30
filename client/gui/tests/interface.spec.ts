import { test, expect } from '@playwright/test';

test('first run, keyboard create flow, browser fallback and responsive layout', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByText('Ожидание Steam')).toBeVisible();
  await page.getByRole('button', { name: 'Создать первую сеть' }).click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await page.getByLabel('Название сети').fill('Minecraft');
  await page.getByRole('dialog').getByRole('button', { name: 'Создать сеть' }).click();
  await expect(page.getByText('Это браузерный просмотр.', { exact: false })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).not.toBeVisible();
  await page.setViewportSize({ width: 1060, height: 720 });
  await page.screenshot({ path: '../../.cache/screenshots/desktop.png', fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
  await page.screenshot({ path: '../../.cache/screenshots/mobile.png', fullPage: true });
  await page.locator('#settings').click();
  await page.getByRole('button', { name: 'Диагностика' }).click();
  await expect(page.getByRole('heading', { name: 'Диагностика' })).toBeVisible();
  await expect(page.getByText('Браузерный просмотр — Steam недоступен')).toBeVisible();
});

test('real state rendering is escaped and adapter action reaches IPC', async ({ page }) => {
  await page.addInitScript(() => {
    const fixture = {
      steam: 'online', steam_id: '76561198000000001', nickname: 'Test owner', relay: 'Ok(Current)',
      networks: [{ id: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', name: 'Minecraft', subnet: '10.77.0.0/24', owner: '76561198000000001', revision: 2, adapter: false,
        members: [{ steam_id: '76561198000000001', name: '<script>unsafe</script>', ip: '10.77.0.1', active: true, state: 'local' }, { steam_id: '76561198000000002', name: 'Test friend', ip: '10.77.0.2', active: true, state: 'offline' }] }],
      friends: [], events: [], sent: 0, received: 0, dropped: 0,
    };
    Object.assign(window, { isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') { fixture.sent++; fixture.received += 2; fixture.dropped++; fixture.networks[0].revision++; return structuredClone(fixture); }
      if (command === 'version') return '0.2.1-preview';
      if (command === 'settings') return { minimize_to_tray: true, check_updates: false };
      if (command === 'save_settings') Object.assign(window, { savedSettings: args });
      if (command === 'check_update') return { configured: false, version: null };
      if (command === 'dispatch') Object.assign(window, { lastCommand: args });
    } } });
  });
  await page.goto('/');
  await expect(page.locator('#brand-version')).toHaveText('0.2.1-preview');
  await expect(page.getByRole('heading', { name: 'Minecraft' })).toBeVisible();
  await expect(page.getByText('<script>unsafe</script>', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Включить', exact: true }).click();
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'set_adapter', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', enabled: true } });
  await page.screenshot({ path: '../../.cache/screenshots/network-fixture.png', fullPage: true });
  await page.locator('#settings').click();
  await page.getByRole('button', { name: 'Диагностика' }).click();
  await page.locator('summary').click();
  await page.locator('summary').focus();
  const oldCounters = await page.locator('#counters').textContent();
  await expect(page.locator('#counters')).not.toHaveText(oldCounters!);
  await expect(page.locator('details')).toHaveAttribute('open', '');
  await expect(page.locator('summary')).toBeFocused();
  await page.getByRole('button', { name: 'Войти по Steam Lobby ID' }).click();
  await page.getByLabel('Steam Lobby ID', { exact: true }).fill('123456789');
  const counters = await page.locator('#counters').textContent();
  await expect(page.locator('#counters')).not.toHaveText(counters!);
  await expect(page.getByLabel('Steam Lobby ID', { exact: true })).toHaveValue('123456789');
  await expect(page.getByLabel('Steam Lobby ID', { exact: true })).toBeFocused();
  await page.keyboard.press('Escape');
  await page.locator('#back-settings').click();
  await page.getByLabel('Сворачивать в трей').uncheck();
  await expect(page.getByLabel('Сворачивать в трей')).not.toBeChecked();
  expect(await page.evaluate(() => (window as unknown as {savedSettings: unknown}).savedSettings)).toEqual({ settings: { minimize_to_tray: false, check_updates: false } });
  await page.getByRole('button', { name: 'Проверить обновления' }).click();
  await expect(page.locator('#update-message')).toContainText('Обновления не настроены');
  await page.screenshot({ path: '../../.cache/screenshots/settings.png', fullPage: true });
  await page.getByRole('link', { name: 'FreeC Tier — мои сети' }).click();
  await page.getByRole('button', { name: 'Удалить сеть с компьютера' }).click();
  await page.getByRole('button', { name: 'Отмена', exact: true }).click();
  await expect(page.getByRole('dialog')).not.toBeVisible();
  await page.getByRole('button', { name: 'Удалить сеть с компьютера' }).click();
  await page.getByRole('button', { name: 'Удалить сеть', exact: true }).click();
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'delete', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289' } });
});
