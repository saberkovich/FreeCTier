import { test, expect } from '@playwright/test';

test('public discovery refreshes without network changes and password reaches IPC', async ({ page }) => {
  await page.addInitScript(() => {
    const fixture = { steam: 'online', steam_id: '2', nickname: 'Guest', relay: 'Ok', networks: [], friends: [], events: [], sent: 0, received: 0, dropped: 0,
      public_networks: [] as { lobby: string; name: string }[], joins: [] as { id: string; name: string; password: boolean }[] };
    Object.assign(window, { isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') return structuredClone(fixture);
      if (command === 'settings') return { theme: 'dark', check_updates: false, minimize_to_tray: true, onboarded: true };
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
      if (command === 'settings') return { minimize_to_tray: true, check_updates: false, theme: 'dark', onboarded: true };
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
  await expect(page.getByText('МОЖЕТ ПРИГЛАШАТЬ · ВЫ')).toHaveCount(0);
  await page.evaluate(() => {
    const { fixture } = window as unknown as { fixture: { networks: { public: boolean; members: { can_invite: boolean }[] }[] } };
    fixture.networks[0].public = false;
    fixture.networks[0].members[1].can_invite = true;
  });
  await expect(page.getByText('Вход только по приглашению')).toBeVisible();
  await expect(page.locator('#invite')).toBeDisabled();
});

test('first run, keyboard create flow, browser fallback and responsive layout', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByText('Steam не подключён')).toBeVisible();
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
      if (command === 'settings') return { minimize_to_tray: true, check_updates: true, autostart: false, theme: 'dark', onboarded: true };
      if (command === 'save_settings') Object.assign(window, { savedSettings: args });
      if (command === 'service_status') return { installed: true, running: true };
      if (command === 'avatars') return { '76561198000000002': 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==' };
      if (command === 'set_autostart') Object.assign(window, { lastAutostart: args });
      if (command === 'check_update') return { configured: true, version: '0.3.0' };
      if (command === 'install_update') Object.assign(window, { installedUpdate: true });
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
  // The header offers the found update on every page and installs it in place.
  await expect(page.locator('#header-update')).toHaveText('Обновить до 0.3.0');
  await page.locator('#header-update').click();
  expect(await page.evaluate(() => (window as unknown as {installedUpdate: unknown}).installedUpdate)).toBe(true);
  await expect(page.locator('#header-update')).toBeHidden();
  await expect(page.getByRole('heading', { name: 'Minecraft' })).toBeVisible();
  // The desktop webview must not show its default right-click menu.
  expect(await page.evaluate(() => {
    const event = new MouseEvent('contextmenu', { cancelable: true });
    document.dispatchEvent(event);
    return event.defaultPrevented;
  })).toBe(true);
  await expect(page.getByText('<script>unsafe</script>', { exact: true })).toBeVisible();
  // The webview runs under `style-src 'self'`, which drops style attributes:
  // anything that needs a per-item color has to come from the stylesheet.
  expect(await page.evaluate(() => [...document.querySelectorAll('[style]')].map(e => e.outerHTML.slice(0, 120)))).toEqual([]);
  // Steam avatars replace the initials once the worker has decoded them.
  await expect(page.locator('#peer-67ae3f2d-0734-47f8-bf1b-e5e2bc64a289-76561198000000002 .avatar img')).toBeVisible();
  await expect(page.locator('#peer-67ae3f2d-0734-47f8-bf1b-e5e2bc64a289-76561198000000001 .avatar img')).toHaveCount(0);
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
  await expect(page.getByText('МОЖЕТ ПРИГЛАШАТЬ')).toBeVisible();
  await page.locator('#access-private').click();
  await expect(page.locator('#access-private')).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByRole('checkbox', { name: 'Приглашать: Test friend' })).not.toBeChecked();
  await expect(page.getByText('МОЖЕТ ПРИГЛАШАТЬ')).toHaveCount(0);
  await page.getByRole('checkbox', { name: 'Приглашать: Test friend' }).check();
  await expect(page.getByRole('checkbox', { name: 'Приглашать: Test friend' })).toBeChecked();
  await expect(page.getByRole('checkbox', { name: 'Исключать: Test friend' })).toBeChecked();
  await expect(page.getByText('МОЖЕТ ПРИГЛАШАТЬ')).toBeVisible();
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
  await expect(page.getByText('Невидимка')).toHaveCount(0);
  await expect(page.getByText('Скрыть игру в библиотеке')).toBeVisible();
  await page.screenshot({ path: '../../.cache/screenshots/steam-help.png' });
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Светлая' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await page.getByLabel('Сворачивать в трей').uncheck();
  await expect(page.getByLabel('Сворачивать в трей')).not.toBeChecked();
  // The service card reports the companion service state; autostart toggles
  // straight through the per-user Run key.
  await expect(page.locator('#service-status')).toHaveText('Установлена и запущена');
  await page.getByLabel('Запускать при входе в Windows').check();
  expect(await page.evaluate(() => (window as unknown as {lastAutostart: unknown}).lastAutostart)).toEqual({ enabled: true });
  await expect(page.getByLabel('Запускать при входе в Windows')).toBeChecked();
  expect(await page.evaluate(() => (window as unknown as {savedSettings: unknown}).savedSettings)).toEqual({ settings: { minimize_to_tray: false, check_updates: true, autostart: true, theme: 'light', onboarded: true } });
  await page.getByRole('button', { name: 'Проверить', exact: true }).click();
  await expect(page.locator('#update-message')).toContainText('Доступна версия 0.3.0');
  await expect(page.getByRole('button', { name: 'Установить 0.3.0' })).toBeVisible();
  await page.screenshot({ path: '../../.cache/screenshots/settings.png', fullPage: true });
  await page.getByRole('link', { name: 'FreeC Tier — мои сети' }).click();
  await page.getByRole('button', { name: 'Удалить сеть с компьютера' }).click();
  await page.getByRole('button', { name: 'Отмена', exact: true }).click();
  await expect(page.getByRole('dialog')).not.toBeVisible();
  await page.getByRole('button', { name: 'Удалить сеть с компьютера' }).click();
  await page.getByRole('button', { name: 'Удалить сеть', exact: true }).click();
  expect(await page.evaluate(() => (window as unknown as {lastCommand: unknown}).lastCommand)).toEqual({ command: { type: 'delete', network: '67ae3f2d-0734-47f8-bf1b-e5e2bc64a289' } });
});

test('first run opens on the Steam library step and follows the license live', async ({ page }) => {
  await page.addInitScript(() => {
    const snapshot = { steam: 'waiting', steam_id: '1', relay: 'unknown', networks: [], friends: [], events: [], sent: 0, received: 0, dropped: 0 };
    const app = { app_id: 324810, in_library: false };
    Object.assign(window, { app, isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') return structuredClone(snapshot);
      if (command === 'settings') return { minimize_to_tray: true, check_updates: false, autostart: false, theme: 'dark' };
      if (command === 'version') return '0.2.0';
      if (command === 'steam_app') return structuredClone(app);
      if (command === 'open_steam_app') Object.assign(window, { opened: args });
      if (command === 'save_settings') Object.assign(window, { savedSettings: args });
      if (command === 'service_status') return { installed: true, running: true };
    } } });
  });
  await page.goto('/');
  const dialog = page.locator('#welcome-dialog');
  await expect(dialog).toBeVisible();
  // Step one names the AppID and says plainly that the license is missing.
  await expect(dialog.getByText('324810')).toBeVisible();
  await expect(dialog.getByText('В библиотеке пока не видно')).toBeVisible();
  // Step two is the same guide Settings offers on its own.
  await expect(dialog.getByText('Скрыть игру в библиотеке')).toBeVisible();
  await dialog.getByRole('button', { name: 'Добавить в Steam' }).click();
  expect(await page.evaluate(() => (window as unknown as {opened: unknown}).opened)).toEqual({ store: false });
  await dialog.getByRole('button', { name: 'Открыть страницу в магазине' }).click();
  expect(await page.evaluate(() => (window as unknown as {opened: unknown}).opened)).toEqual({ store: true });
  // The user acts in Steam; the window notices on its own.
  await page.evaluate(() => { (window as unknown as { app: { in_library: boolean } }).app.in_library = true; });
  await expect(dialog.getByText('уже в библиотеке')).toBeVisible();
  await page.screenshot({ path: '../../.cache/screenshots/welcome.png' });
  await dialog.getByRole('button', { name: 'Готово' }).click();
  await expect(dialog).not.toBeVisible();
  expect(await page.evaluate(() => (window as unknown as {savedSettings: {settings: {onboarded: boolean}}}).savedSettings.settings.onboarded)).toBe(true);
  // Reopening it from Settings must not need a restart.
  await page.locator('#settings').click();
  await page.getByRole('button', { name: 'Настройка Steam' }).click();
  await expect(dialog).toBeVisible();
});

test('accent swatches recolor the app, persist and hand over to the custom editor', async ({ page }) => {
  await page.addInitScript(() => {
    const fixture = { steam: 'online', steam_id: '2', nickname: 'Guest', relay: 'Ok', networks: [], friends: [], events: [], sent: 0, received: 0, dropped: 0 };
    Object.assign(window, { isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') return structuredClone(fixture);
      if (command === 'settings') return { minimize_to_tray: true, check_updates: false, autostart: false, theme: 'dark', onboarded: true };
      if (command === 'version') return '0.2.0';
      if (command === 'save_settings') Object.assign(window, { savedSettings: args });
      if (command === 'service_status') return { installed: false, running: false };
    } } });
  });
  await page.goto('/');
  await page.locator('#settings').click();
  // With no override the ring sits on the preset accent of the active theme.
  await expect(page.getByRole('button', { name: 'Акцент: По умолчанию' })).toHaveAttribute('aria-pressed', 'true');
  await page.getByRole('button', { name: 'Акцент: Фиолетовый' }).click();
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--accent').trim())).toBe('#8b5cf6');
  await expect(page.getByRole('button', { name: 'Акцент: Фиолетовый' })).toHaveAttribute('aria-pressed', 'true');
  expect(await page.evaluate(() => (window as unknown as {savedSettings: unknown}).savedSettings)).toEqual({
    settings: { minimize_to_tray: true, check_updates: false, autostart: false, theme: 'dark', onboarded: true, accent: '#8b5cf6' },
  });
  // The light preset keeps the chosen accent, the rest of the palette flips.
  await page.getByRole('button', { name: 'Светлая' }).click();
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--accent').trim())).toBe('#8b5cf6');
  // «Своя» owns every token, so the swatch row steps aside and seeds from it.
  await page.getByRole('button', { name: 'Своя' }).click();
  await expect(page.getByRole('button', { name: 'Акцент: Фиолетовый' })).toHaveCount(0);
  expect(await page.evaluate(() => {
    const saved = (window as unknown as {savedSettings?: {settings?: {custom_colors?: Record<string, string>}}}).savedSettings;
    return saved?.settings?.custom_colors?.accent;
  })).toBe('#8b5cf6');
  // Back on a preset, «По умолчанию» drops the override for good.
  await page.getByRole('button', { name: 'Тёмная' }).click();
  await page.getByRole('button', { name: 'Акцент: По умолчанию' }).click();
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--accent').trim())).toBe('#e03650');
  await page.waitForTimeout(400); // let the switch finish its color transition
  await page.screenshot({ path: '../../.cache/screenshots/accent.png', fullPage: true });
});

test('custom theme seeds from the current theme, previews live and persists', async ({ page }) => {
  await page.addInitScript(() => {
    const fixture = { steam: 'online', steam_id: '2', nickname: 'Guest', relay: 'Ok', networks: [], friends: [], events: [], sent: 0, received: 0, dropped: 0 };
    Object.assign(window, { isTauri: true, __TAURI_INTERNALS__: { invoke: async (command: string, args: unknown) => {
      if (command === 'snapshot') return structuredClone(fixture);
      if (command === 'settings') return { minimize_to_tray: true, check_updates: false, autostart: false, theme: 'dark', onboarded: true };
      if (command === 'version') return '0.2.0';
      if (command === 'save_settings') Object.assign(window, { savedSettings: args });
      if (command === 'service_status') return { installed: false, running: false };
    } } });
  });
  await page.goto('/');
  await page.locator('#settings').click();
  await page.getByRole('button', { name: 'Своя' }).click();
  // Seeded palette covers every token, so the editor starts from the dark theme.
  await expect(page.locator('#theme-editor .color-input')).toHaveCount(16);
  expect(await page.evaluate(() => (window as unknown as {savedSettings: unknown}).savedSettings)).toEqual({
    settings: { minimize_to_tray: true, check_updates: false, autostart: false, theme: 'custom', onboarded: true, custom_colors: expect.objectContaining({ accent: '#e03650', bg: '#0d0608' }) },
  });
  // Picking previews immediately as an inline variable on <html>.
  await page.locator('.color-input[data-token="accent"]').fill('#00ff00');
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'custom');
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--accent').trim())).toBe('#00ff00');
  // The value persists when the picker closes.
  await expect.poll(async () => await page.evaluate(() => {
    const saved = (window as unknown as {savedSettings?: {settings?: {custom_colors?: Record<string, string>}}}).savedSettings;
    return saved?.settings?.custom_colors?.accent;
  })).toBe('#00ff00');
  // Reset returns to the plain dark theme without overrides.
  await page.getByRole('button', { name: 'Сбросить' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await expect(page.locator('#theme-editor')).toHaveCount(0);
  expect(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--accent').trim())).toBe('#e03650');
});
