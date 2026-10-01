import { test, expect } from '@playwright/test';

test('public discovery refreshes without network changes and password reaches IPC', async ({ page }) => {
  await page.addInitScript(() => {
    const fixture = { steam: 'online', steam_id: '2', nickname: 'Guest', relay: 'Ok', networks: [], friends: [], events: [], sent: 0, received: 0, dropped: 0,
      public_networks: [] as { lobby: string; name: string }[], joins: [] as { id: string; name: string; password: boolean }[] };
    Object.assign(window, { isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') return structuredClone(fixture);
      if (command === 'settings') return { theme: 'dark', check_updates: false, minimize_to_tray: true };
      if (command === 'version') return '0.2.0';
      if (command === 'dispatch') {
        Object.assign(window, { lastCommand: args });
        const action = (args as { command: { type: string } }).command;
        if (action.type === 'discover') fixture.public_networks = [{ lobby: '1234', name: '<Public LAN>' }];
        if (action.type === 'join') fixture.joins = [{ id: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', name: '<Public LAN>', password: true }];
        if (action.type === 'submit_password') throw new Error('Неверный пароль сети');
      }
    } } });
  });
  await page.goto('/');
  await page.locator('#discover').click();
  await expect(page.getByText('<Public LAN>', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Войти', exact: true }).click();
  await page.getByRole('button', { name: 'Ввести пароль' }).waitFor();
  await page.screenshot({ path: '../../.cache/screenshots/discovery.png', fullPage: true });
  await page.getByRole('button', { name: 'Ввести пароль' }).click();
  await page.locator('#password-input').fill('wrong password');
  await page.waitForTimeout(1200);
  await expect(page.locator('#password-input')).toHaveValue('wrong password');
  await page.locator('#password-dialog').getByRole('button', { name: 'Продолжить' }).click();
  await expect(page.locator('#password-dialog')).toBeVisible();
  await expect(page.locator('#notice')).toContainText('Неверный пароль сети');
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'submit_password', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', password: 'wrong password' } });
  await page.keyboard.press('Escape');
  // The pending admission is cancellable and no longer hijacks navigation.
  await page.getByRole('button', { name: 'Отмена', exact: true }).click();
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'cancel_join', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289' } });
  await page.locator('#settings').click();
  await expect(page.locator('#settings-page')).toBeVisible();
  const bar = page.locator('#joins-bar');
  await expect(bar).toBeVisible();
  await expect(bar.locator('.join-chip')).toHaveCount(1);
  await bar.getByRole('button', { name: 'Пароль', exact: true }).click();
  await expect(page.locator('#password-dialog')).toBeVisible();
  await page.keyboard.press('Escape');
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
});

test('public member invitations follow effective permission without owner controls', async ({ page }) => {
  await page.addInitScript(() => {
    const fixture = {
      steam: 'online', steam_id: '76561198000000002', nickname: 'Member', relay: 'Ok(Current)',
      networks: [{ id: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', name: 'Public LAN', subnet: '10.77.0.0/24', owner: '76561198000000001', revision: 2, adapter: false, public: true, can_invite: true,
        members: [{ steam_id: '76561198000000001', name: 'Owner', ip: '10.77.0.1', active: true, state: 'offline', can_invite: true, may_invite: true }, { steam_id: '76561198000000002', name: 'Member', ip: '10.77.0.2', active: true, state: 'local', can_invite: true, may_invite: true }] }],
      friends: [{ steam_id: '76561198000000003', name: 'New friend' }], events: [], sent: 0, received: 0, dropped: 0,
    };
    Object.assign(window, { fixture, isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') return structuredClone(fixture);
      if (command === 'version') return '0.2.1-preview';
      if (command === 'settings') return { minimize_to_tray: true, check_updates: false, theme: 'dark' };
      if (command === 'dispatch') Object.assign(window, { lastCommand: args });
    } } });
  });
  await page.goto('/');
  await expect(page.locator('#invite')).toBeEnabled();
  await expect(page.locator('#access-public')).toHaveCount(0);
  await expect(page.locator('[data-invite]')).toHaveCount(0);
  await page.locator('#invite').click();
  await expect(page.locator('#invite-dialog')).toBeVisible();
  await page.locator('[data-friend]').click();
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'invite_friend', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', steam_id: '76561198000000003' } });
  await expect(page.locator('#invite-dialog')).not.toBeVisible();
  await page.evaluate(() => {
    const { fixture } = window as unknown as { fixture: { networks: { can_invite: boolean; members: { can_invite: boolean; may_invite: boolean }[] }[] } };
    fixture.networks[0].can_invite = false;
    fixture.networks[0].members[1].can_invite = false;
    fixture.networks[0].members[1].may_invite = false;
  });
  await expect(page.locator('#invite')).toBeDisabled();
  await expect(page.getByText('Участник · может приглашать · Вы')).toHaveCount(0);
  await page.evaluate(() => {
    const { fixture } = window as unknown as { fixture: { networks: { public: boolean; members: { can_invite: boolean }[] }[] } };
    fixture.networks[0].public = false;
    fixture.networks[0].members[1].can_invite = true;
  });
  await expect(page.getByText('Вход по приглашению')).toBeVisible();
  await expect(page.locator('#invite')).toBeDisabled();
});

test('first run, keyboard create flow, browser fallback and responsive layout', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByText('Steam не запущен')).toBeVisible();
  await page.getByRole('button', { name: 'Создать первую сеть' }).click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await page.getByLabel('Название').fill('Minecraft');
  await page.getByRole('dialog').getByRole('button', { name: 'Публичная' }).click();
  await expect(page.getByRole('dialog').getByRole('button', { name: 'Публичная' })).toHaveClass(/active/);
  await page.screenshot({ path: '../../.cache/screenshots/create-dialog.png' });
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
  // Browser preview keeps the normal context menu.
  expect(await page.evaluate(() => {
    const event = new MouseEvent('contextmenu', { cancelable: true });
    document.dispatchEvent(event);
    return event.defaultPrevented;
  })).toBe(false);
});

test('real state rendering is escaped and adapter action reaches IPC', async ({ page }) => {
  await page.addInitScript(() => {
    const fixture = {
      steam: 'online', steam_id: '76561198000000001', nickname: 'Test owner', relay: 'Ok(Current)',
      networks: [{ id: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', name: 'Minecraft', subnet: '10.77.0.0/24', owner: '76561198000000001', revision: 2, adapter: false, public: false, can_invite: true,
        members: [{ steam_id: '76561198000000001', name: '<script>unsafe</script>', ip: '10.77.0.1', active: true, state: 'local', can_invite: true, may_invite: true, can_kick: true }, { steam_id: '76561198000000002', name: 'Test friend', ip: '10.77.0.2', active: true, state: 'offline', can_invite: false, may_invite: false, can_kick: false }] }],
      friends: [], events: [], sent: 0, received: 0, dropped: 0,
    };
    Object.assign(window, { isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') { fixture.sent++; fixture.received += 2; fixture.dropped++; fixture.networks[0].revision++; return structuredClone(fixture); }
      if (command === 'version') return '0.2.1-preview';
      if (command === 'settings') return { minimize_to_tray: true, check_updates: false, theme: 'dark' };
      if (command === 'save_settings') Object.assign(window, { savedSettings: args });
      if (command === 'check_update') return { configured: false, version: null };
      if (command === 'dispatch') {
        Object.assign(window, { lastCommand: args });
        const action = (args as { command: { type: string; public?: boolean; can_invite?: boolean; can_kick?: boolean } }).command;
        const network = fixture.networks[0];
        if (action.type === 'set_access') { network.public = action.public!; network.members[1].can_invite = network.public; }
        if (action.type === 'set_permissions') { network.members[1].can_invite = action.can_invite!; network.members[1].can_kick = action.can_kick!; }
        network.members[1].may_invite = network.members[1].can_invite;
      }
    } } });
  });
  await page.goto('/');
  await expect(page.locator('#brand-version')).toHaveText('0.2.1-preview');
  await expect(page.getByRole('heading', { name: 'Minecraft' })).toBeVisible();
  // The desktop webview must not show its default right-click menu.
  expect(await page.evaluate(() => {
    const event = new MouseEvent('contextmenu', { cancelable: true });
    document.dispatchEvent(event);
    return event.defaultPrevented;
  })).toBe(true);
  await expect(page.getByText('<script>unsafe</script>', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Включить', exact: true }).click();
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'set_adapter', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', enabled: true } });
  await page.getByRole('button', { name: 'Публичная' }).click();
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'set_access', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', public: true } });
  await expect(page.locator('#access-public')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByRole('checkbox', { name: 'Приглашать: Test friend' })).toBeChecked();
  await page.getByRole('checkbox', { name: 'Исключать: Test friend' }).check();
  await page.screenshot({ path: '../../.cache/screenshots/detail.png', fullPage: true });
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'set_permissions', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289', steam_id: '76561198000000002', can_invite: true, can_kick: true } });
  await expect(page.getByRole('checkbox', { name: 'Исключать: Test friend' })).toBeChecked();
  await expect(page.getByText('Участник · может приглашать')).toBeVisible();
  await page.locator('#access-private').click();
  await expect(page.locator('#access-private')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByRole('checkbox', { name: 'Приглашать: Test friend' })).not.toBeChecked();
  await expect(page.getByText('Участник · может приглашать')).toHaveCount(0);
  await page.getByRole('checkbox', { name: 'Приглашать: Test friend' }).check();
  await expect(page.getByRole('checkbox', { name: 'Приглашать: Test friend' })).toBeChecked();
  await expect(page.getByRole('checkbox', { name: 'Исключать: Test friend' })).toBeChecked();
  await expect(page.getByText('Участник · может приглашать')).toBeVisible();
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
  await page.getByRole('button', { name: 'Скрыть статус' }).click();
  await expect(page.getByRole('dialog')).toBeVisible();
  await expect(page.getByText('Приватность профиля')).toBeVisible();
  await expect(page.getByText('Скрыть игру в библиотеке')).toBeVisible();
  await page.screenshot({ path: '../../.cache/screenshots/steam-help.png' });
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Светлая' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await page.getByLabel('Сворачивать в трей').uncheck();
  await expect(page.getByLabel('Сворачивать в трей')).not.toBeChecked();
  expect(await page.evaluate(() => (window as unknown as {savedSettings: unknown}).savedSettings)).toEqual({ settings: { minimize_to_tray: false, check_updates: false, theme: 'light' } });
  await page.getByRole('button', { name: 'Проверить', exact: true }).click();
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
