import { invoke, isTauri } from '@tauri-apps/api/core';
import morphdom from 'morphdom';
import './styles.css';

type Peer = { steam_id: string; name: string; ip: string; active: boolean; state: string; ping_ms?: number; can_invite: boolean; may_invite: boolean; can_kick: boolean; may_kick: boolean };
type Network = { id: string; name: string; subnet: string; owner: string; revision: number; adapter: boolean; lobby?: string; public: boolean; can_invite: boolean; password: boolean; members: Peer[] };
type Snapshot = { steam: string; steam_id?: string; nickname?: string; relay: string; networks: Network[]; friends: { steam_id: string; name: string }[]; events: string[]; received: number; sent: number; dropped: number; joins?: { id: string; name: string; password: boolean }[]; public_networks?: { lobby: string; name: string }[] };
type Command = { type: string; [key: string]: unknown };
type Settings = { minimize_to_tray: boolean; check_updates: boolean; theme: string };

let state: Snapshot = { steam: 'waiting', relay: 'unknown', networks: [], friends: [], events: [], received: 0, sent: 0, dropped: 0 };
let selected: string | undefined;
let previous = '';
let page: 'networks' | 'settings' | 'diagnostics' | 'discover' = 'networks';
let preferences: Settings = { minimize_to_tray: true, check_updates: true, theme: 'dark' };
let updateMessage = 'Проверок ещё не было.';
let updateVersion: string | null = null;
let updating = false;
let busy = false;
let appVersion = '';
let createPublic = false;
const app = document.querySelector<HTMLDivElement>('#app')!;
app.innerHTML = `
  <header class="topbar"><a class="brand" href="#" aria-label="FreeC Tier — мои сети"><span class="brand-mark">F</span><span class="brand-name">FreeC Tier</span><small class="brand-version" id="brand-version"></small></a><div id="session" role="status"></div></header>
  <div class="workspace"><aside class="sidebar"><div class="rail-heading"><h2>Мои сети</h2><span id="network-count">0</span></div><nav id="network-list" aria-label="Сохранённые сети"></nav>
  <button class="button primary create" id="create">Создать сеть</button>
  <div class="sidebar-bottom"><button class="nav-button" id="settings">Настройки <span aria-hidden="true">⚙</span></button></div></aside>
  <main id="content"></main></div>
  <div id="notice" role="status" hidden></div>
  <div id="joins-bar" role="status" hidden></div>
  <dialog id="create-dialog"><form id="create-form"><div class="dialog-heading"><h2>Новая сеть</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><label for="network-name">Название</label><input id="network-name" name="name" maxlength="64" required placeholder="Например, Minecraft" autocomplete="off" /><span>Доступ</span><div class="segmented" role="group" aria-label="Доступ"><button type="button" id="create-private" class="seg active" aria-pressed="true">Приватная</button><button type="button" id="create-public" class="seg" aria-pressed="false">Публичная</button></div><p class="hint">В публичной сети участники с разрешением могут приглашать друзей.</p><label>Подсеть</label><div class="readonly">Автоматически <span>10.77.x.0/24</span></div><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button primary" type="submit">Создать сеть</button></div></form></dialog>
  <dialog id="join-dialog"><form id="join-form"><div class="dialog-heading"><h2>Подключиться по lobby</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><label for="lobby-id">Steam Lobby ID</label><input id="lobby-id" name="lobby" required inputmode="numeric" pattern="[0-9]{1,20}" placeholder="64-битный идентификатор" /><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button primary" type="submit">Присоединиться</button></div></form></dialog>
  <dialog id="invite-dialog"><div class="dialog-heading"><h2>Пригласить друга</h2><button class="icon-button close" aria-label="Закрыть">×</button></div><div id="friend-list"></div><button id="overlay" class="button">Открыть список в Steam</button></dialog>
  <dialog id="delete-dialog"><form id="delete-form"><div class="dialog-heading"><h2>Удалить сеть?</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p id="delete-description"></p><p>У остальных участников сеть останется.</p><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button danger" type="submit">Удалить сеть</button></div></form></dialog>
  <dialog id="steam-help-dialog"><div class="dialog-heading"><h2>Скрыть статус в Steam</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p class="steam-help-note">Приложение работает через Steam, поэтому Steam показывает запущенную игру. Отключить это можно только в своём аккаунте — любым из способов.</p><div class="steam-help">
    <section class="step"><span class="step-num">1</span><div class="step-body"><h3>Невидимка</h3><p>Кликните по своему имени в Steam и выберите «Невидимка».</p><img class="step-shot" src="/steam/invisible.png" alt="Меню статуса Steam с пунктом «Невидимка»" loading="lazy" /></div></section>
    <section class="step"><span class="step-num">2</span><div class="step-body"><h3>Приватность профиля</h3><p>Профиль → Изменить профиль → «Доступ к игровой информации» → «Скрытый».</p><img class="step-shot" src="/steam/privacy.png" alt="Приватность профиля Steam" loading="lazy" /><button class="button" id="open-privacy">Открыть настройки приватности</button></div></section>
    <section class="step"><span class="step-num">3</span><div class="step-body"><h3>Скрыть игру в библиотеке</h3><p>Библиотека → правый клик по игре → «Управление» → «Сделать приватной».</p><img class="step-shot" src="/steam/library.png" alt="Контекстное меню игры в библиотеке Steam" loading="lazy" /></div></section>
  </div></dialog>`;

const content = document.querySelector<HTMLElement>('#content')!;
document.querySelector('#create')!.insertAdjacentHTML('afterend', '<button class="button" id="discover">Публичные сети</button>');
document.querySelector('#create-public')!.closest('.segmented')!.insertAdjacentHTML('afterend', '<label id="create-password-label" hidden>Пароль (необязательно)<input id="create-password" type="password" maxlength="128" autocomplete="new-password" /></label>');
document.querySelector('#app')!.insertAdjacentHTML('beforeend', `<dialog id="password-dialog"><form id="password-form"><div class="dialog-heading"><h2 id="password-title">Пароль сети</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><label for="password-input">Пароль</label><input id="password-input" type="password" maxlength="128" autocomplete="current-password" /><p class="hint" id="password-hint"></p><div class="dialog-actions"><button class="button primary" type="submit">Продолжить</button></div></form></dialog>`);
// Keyed DOM reconciliation keeps focus, open details and scroll containers alive.
function patch(element: HTMLElement, html: string) {
  const next = element.cloneNode(false) as HTMLElement; next.innerHTML = html;
  morphdom(element, next, { childrenOnly: true, onBeforeElUpdated(from, to) {
    if (from instanceof HTMLDetailsElement) to.toggleAttribute('open', from.open);
    if (from.id === 'event-log' || from.id === 'counters') return false;
    return !from.isEqualNode(to);
  } });
}
function contentHTML(html: string) { patch(content, html); }
function escape(value: string) { return value.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]!)); }
const labels: Record<string, string> = { connected: 'Подключён', connecting: 'Подключение…', reconnecting: 'Переподключение…', offline: 'Не в сети', local: 'Этот компьютер', removed: 'Доступ отозван' };
function status(value: string) { return `<span class="peer-status ${escape(value)}"><i aria-hidden="true"></i>${escape(labels[value] || value)}</span>`; }
function network() { return state.networks.find(n => n.id === selected); }
function appLabel() { return appVersion ? `FreeC Tier ${appVersion}` : 'FreeC Tier'; }
function applyTheme(theme: string) { document.documentElement.dataset.theme = theme === 'light' ? 'light' : 'dark'; }
function setVersion(value: string) {
  appVersion = value;
  const brand = document.querySelector('#brand-version'); if (brand) brand.textContent = value;
}
function notice(message: string) { const el = document.querySelector<HTMLElement>('#notice')!; el.textContent = message; el.hidden = false; setTimeout(() => { el.hidden = true; }, 7000); }
async function send(command: Command): Promise<boolean> {
  if (!isTauri()) { notice('Это браузерный просмотр. Для работы со Steam запустите desktop-клиент.'); return false; }
  if (busy) return false;
  busy = true;
  try { await invoke('dispatch', { command }); return true; }
  catch (error) { notice(String(error)); return false; }
  finally { busy = false; }
}

function render() {
  renderJoinsBar();
  const online = state.steam === 'online';
  patch(document.querySelector<HTMLElement>('#session')!, `<span class="steam-pill ${online ? 'online' : 'offline'}"><i aria-hidden="true"></i>${online ? 'Steam подключён' : 'Steam не запущен'}</span>${state.nickname ? `<span class="account">${escape(state.nickname)}</span>` : ''}`);
  document.querySelector('#network-count')!.textContent = String(state.networks.length);
  patch(document.querySelector<HTMLElement>('#network-list')!, state.networks.length ? state.networks.map(n => `<button id="network-${n.id}" class="network-item ${n.id === selected && page === 'networks' ? 'selected' : ''}" data-select="${n.id}" ${n.id === selected && page === 'networks' ? 'aria-current="page"' : ''}><span class="network-icon ${n.adapter ? 'active' : ''}" aria-hidden="true"></span><span><strong>${escape(n.name)}</strong><small>${n.adapter ? 'Включена' : 'Выключена'}</small></span></button>`).join('') : '<p class="rail-empty">Пока пусто</p>');
  document.querySelectorAll<HTMLButtonElement>('[data-select]').forEach(button => button.onclick = () => { selected = button.dataset.select; page = 'networks'; render(); });
  if (page === 'diagnostics') { renderDiagnostics(); return; }
  if (page === 'settings') { renderSettings(); return; }
  if (page === 'discover') { renderDiscovery(); return; }
  const active = network();
  if (!active) {
    contentHTML(`<section class="empty-state" id="empty"><svg class="empty-art" viewBox="0 0 180 110" role="img" aria-hidden="true"><path d="M42 48 L90 60 M138 48 L90 60" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" /><rect x="20" y="14" width="44" height="34" rx="9" fill="none" stroke="currentColor" stroke-width="2" /><rect x="116" y="14" width="44" height="34" rx="9" fill="none" stroke="currentColor" stroke-width="2" /><rect x="66" y="60" width="48" height="38" rx="10" fill="var(--accent)" /></svg><h1>Объедините компьютеры в одну сеть</h1><p>Как в одной локальной сети — для игр и любых LAN-приложений.</p><button class="button primary" id="create-first">Создать первую сеть</button></section>`);
    document.querySelector<HTMLButtonElement>('#create-first')!.onclick = openCreate;
    return;
  }
  const owner = active.owner === state.steam_id;
  const local = active.members.find(m => m.steam_id === state.steam_id);
  contentHTML(`<section class="page" id="detail-${active.id}"><div class="page-head"><div><h1>${escape(active.name)}</h1><p class="page-sub">Участников: ${active.members.filter(m => m.active).length} · ${active.public ? 'Публичная' : 'Приватная'}</p></div><div class="page-actions"><button id="invite" class="button" ${!active.can_invite || !online ? 'disabled' : ''}>Пригласить</button></div></div>
    <div class="status-card ${active.adapter ? 'on' : ''}"><span class="status-dot" aria-hidden="true"></span><div class="status-text"><strong>${active.adapter ? 'Сеть включена' : 'Сеть выключена'}</strong><span>${local?.active ? `Ваш адрес ${escape(local.ip)}` : 'Доступ отозван'}</span></div><button id="toggle-adapter" class="button ${active.adapter ? '' : 'primary'}" ${!online || !local?.active ? 'disabled' : ''}>${active.adapter ? 'Выключить' : 'Включить'}</button></div>
    ${!online ? '<p class="hint">Steam не запущен — включение сети и приглашения недоступны.</p>' : ''}
    <div class="section-head"><h2>Доступ</h2></div><div class="card"><div class="row"><span class="row-label">Тип сети<small>${active.public ? 'Доступна в списке публичных сетей' : 'Вход по приглашению'}</small></span>${owner ? `<div class="segmented" role="group" aria-label="Доступ"><button id="access-private" class="seg ${active.public ? '' : 'active'}" aria-pressed="${!active.public}" ${online ? '' : 'disabled'}>Приватная</button><button id="access-public" class="seg ${active.public ? 'active' : ''}" aria-pressed="${active.public}" ${online ? '' : 'disabled'}>Публичная</button></div>` : `<span class="muted">${active.public ? 'Публичная' : 'Приватная'}</span>`}</div>${active.public ? `<div class="row"><span class="row-label">Пароль<small>${active.password ? 'Установлен' : 'Без пароля'}</small></span>${owner ? `<button class="button" id="set-password" ${online ? '' : 'disabled'}>${active.password ? 'Изменить' : 'Установить'}</button>` : ''}</div>` : ''}</div>
    <div class="section-head"><h2>Участники</h2></div><div class="table-wrap"><table><thead><tr><th>Участник</th><th>IP-адрес</th><th>Соединение</th><th><span class="sr-only">Права и действия</span></th></tr></thead><tbody>${active.members.map(peer => `<tr id="peer-${active.id}-${peer.steam_id}" class="${peer.active ? '' : 'revoked'}"><td><div class="person"><span class="initials" aria-hidden="true">${escape((peer.name || '?').slice(0, 2).toUpperCase())}</span><span><strong>${escape(peer.name || peer.steam_id)}</strong><small>${peer.steam_id === active.owner ? 'Владелец' : peer.may_invite ? 'Участник · может приглашать' : 'Участник'}${peer.steam_id === state.steam_id ? ' · Вы' : ''}</small></span></div></td><td><code>${escape(peer.ip)}</code></td><td>${status(peer.state)}${peer.ping_ms != null ? `<small>${peer.ping_ms} мс</small>` : ''}</td><td><div class="table-actions">${owner && peer.steam_id !== active.owner ? `<label class="permission"><input type="checkbox" data-permission="invite" data-peer="${peer.steam_id}" aria-label="Приглашать: ${escape(peer.name || peer.steam_id)}" ${peer.can_invite ? 'checked' : ''} ${online ? '' : 'disabled'} />Приглашать</label><label class="permission"><input type="checkbox" data-permission="kick" data-peer="${peer.steam_id}" aria-label="Исключать: ${escape(peer.name || peer.steam_id)}" ${peer.can_kick ? 'checked' : ''} ${online ? '' : 'disabled'} />Исключать</label>` : ''}${(owner && peer.steam_id !== active.owner) || (peer.may_kick && peer.active) ? `<button class="text-button danger" data-member="${peer.steam_id}" data-active="${peer.active}" ${online ? '' : 'disabled'}>${peer.active ? 'Удалить' : 'Вернуть'}</button>` : ''}</div></td></tr>`).join('')}</tbody></table></div>
    <div class="danger-zone"><button class="text-button danger" id="delete-network">Удалить сеть с компьютера</button></div></section>`);
  document.querySelector<HTMLButtonElement>('#invite')!.onclick = openInvite;
  document.querySelector<HTMLButtonElement>('#toggle-adapter')!.onclick = () => { void send({ type: 'set_adapter', network: active.id, enabled: !active.adapter }); };
  const privateAccess = document.querySelector<HTMLButtonElement>('#access-private');
  const publicAccess = document.querySelector<HTMLButtonElement>('#access-public');
  if (privateAccess) privateAccess.onclick = () => { void send({ type: 'set_access', network: active.id, public: false }); };
  if (publicAccess) publicAccess.onclick = () => { void send({ type: 'set_access', network: active.id, public: true }); };
  document.querySelectorAll<HTMLInputElement>('[data-permission]').forEach(input => input.onchange = () => {
    // Read both checkboxes from the row, not the render-time snapshot: a quick
    // second toggle must not resurrect the stale value of the first one.
    const row = input.closest('.table-actions')!;
    const invite = row.querySelector<HTMLInputElement>('[data-permission="invite"]')!;
    const kick = row.querySelector<HTMLInputElement>('[data-permission="kick"]')!;
    void send({ type: 'set_permissions', network: active.id, steam_id: input.dataset.peer, can_invite: invite.checked, can_kick: kick.checked });
  });
  const passwordButton = document.querySelector<HTMLButtonElement>('#set-password');
  if (passwordButton) passwordButton.onclick = () => openPassword(active.id, true);
  document.querySelector<HTMLButtonElement>('#delete-network')!.onclick = () => {
    const dialog = document.querySelector<HTMLDialogElement>('#delete-dialog')!;
    dialog.dataset.network = active.id;
    document.querySelector('#delete-description')!.textContent = owner
      ? `«${active.name}» будет удалена с этого компьютера. Как владелец вы потеряете управление сетью, если не сохраните её конфигурацию.`
      : `«${active.name}» будет удалена с этого компьютера.`;
    dialog.showModal();
  };
  document.querySelectorAll<HTMLButtonElement>('[data-member]').forEach(button => button.onclick = () => send({ type: button.dataset.active === 'true' ? 'revoke' : 'readmit', network: active.id, steam_id: button.dataset.member }));
}

function openPassword(id: string, editing: boolean) {
  const dialog = document.querySelector<HTMLDialogElement>('#password-dialog')!;
  dialog.dataset.network = id; dialog.dataset.editing = String(editing);
  document.querySelector('#password-title')!.textContent = editing ? 'Изменить пароль' : 'Вход в сеть';
  document.querySelector('#password-hint')!.textContent = editing ? 'Пустое поле удалит пароль. Уже принятые участники сохранят доступ.' : 'Пароль проверяется автоматически. Владелец может быть офлайн.';
  document.querySelector<HTMLInputElement>('#password-input')!.value = '';
  dialog.showModal();
}
// Pending admissions follow the user across pages instead of forcing a page.
function renderJoinsBar() {
  const bar = document.querySelector<HTMLDivElement>('#joins-bar')!;
  const joins = page === 'discover' ? [] : state.joins || [];
  bar.hidden = !joins.length;
  if (!joins.length) return;
  bar.innerHTML = joins.map(j => `<div class="join-chip"><span>${escape(j.name)}</span><small>${j.password ? 'Требуется пароль' : 'Подключение…'}</small>${j.password ? `<button class="button" data-password="${j.id}">Пароль</button>` : ''}<button class="text-button" data-cancel-join="${j.id}">Отмена</button></div>`).join('');
  document.querySelectorAll<HTMLButtonElement>('[data-password]').forEach(b => b.onclick = () => openPassword(b.dataset.password!, false));
  document.querySelectorAll<HTMLButtonElement>('[data-cancel-join]').forEach(b => b.onclick = () => { void send({ type: 'cancel_join', network: b.dataset.cancelJoin }); });
}
function renderDiscovery() {
  const joins = state.joins || [];
  contentHTML(`<section class="page" id="discovery-page"><div class="page-head"><h1>Публичные сети</h1><button class="button" id="refresh-public" ${state.steam === 'online' ? '' : 'disabled'}>Обновить</button></div>${joins.map(j => `<div class="card row" id="pending-${j.id}"><span>${escape(j.name)}<small>${j.password ? 'Требуется пароль' : 'Подключение…'}</small></span><div class="row-actions">${j.password ? `<button class="button primary" data-password="${j.id}">Ввести пароль</button>` : ''}<button class="text-button" data-cancel-join="${j.id}">Отмена</button></div></div>`).join('')}<div class="card">${(state.public_networks || []).map(n => `<div class="row"><span class="row-label">${escape(n.name)}</span><button class="button" data-lobby="${n.lobby}">Войти</button></div>`).join('') || '<p class="hint">Сети не найдены. Нажмите «Обновить», чтобы повторить поиск.</p>'}</div></section>`);
  document.querySelector<HTMLButtonElement>('#refresh-public')!.onclick = () => { void send({ type: 'discover' }); };
  document.querySelectorAll<HTMLButtonElement>('[data-lobby]').forEach(b => b.onclick = () => { void send({ type: 'join', lobby: b.dataset.lobby }); });
  document.querySelectorAll<HTMLButtonElement>('[data-password]').forEach(b => b.onclick = () => openPassword(b.dataset.password!, false));
  document.querySelectorAll<HTMLButtonElement>('[data-cancel-join]').forEach(b => b.onclick = () => { void send({ type: 'cancel_join', network: b.dataset.cancelJoin }); });
}

function renderDiagnostics() {
  contentHTML(`<section class="page" id="diagnostic-page"><button id="back-settings" class="text-button back">← Настройки</button><div class="page-head"><div><h1>Диагностика</h1><p class="page-sub">Состояние клиента и события</p></div></div><div class="panel"><dl class="dl"><dt>SteamID</dt><dd><code>${escape(state.steam_id || 'Не получен')}</code></dd><dt>Steam Relay</dt><dd>${escape(state.relay)}</dd><dt>Транспорт</dt><dd>Steam P2P · ICE отключён</dd><dt>Адаптер</dt><dd>Wintun · IPv4 · MTU 1280</dd><dt>Среда</dt><dd>${isTauri() ? 'Desktop-клиент' : 'Браузерный просмотр — Steam недоступен'}</dd></dl></div><p class="muted counters" id="counters"></p>${state.networks.map(n => `<details class="net" id="info-${n.id}"><summary>${escape(n.name)}</summary><dl class="dl"><dt>Network ID</dt><dd><code>${n.id}</code></dd><dt>Подсеть</dt><dd>${escape(n.subnet)}</dd><dt>Версия</dt><dd>${n.revision}</dd><dt>Lobby ID</dt><dd><code>${n.lobby || '—'}</code></dd></dl></details>`).join('')}<div class="section-head"><h2>Журнал событий</h2></div><div class="event-log" id="event-log"></div><div class="page-actions"><button id="join" class="button">Войти по Steam Lobby ID</button></div></section>`);
  document.querySelector<HTMLButtonElement>('#back-settings')!.onclick = () => { page = 'settings'; render(); };
  document.querySelector<HTMLButtonElement>('#join')!.onclick = () => document.querySelector<HTMLDialogElement>('#join-dialog')!.showModal();
  updateDynamic();
}
function updateDynamic() {
  const counters = document.querySelector('#counters');
  if (counters) counters.textContent = `Отправлено ${state.sent} · Получено ${state.received} · Отклонено ${state.dropped}`;
  const log = document.querySelector<HTMLElement>('#event-log');
  if (log) patch(log, state.events.length ? state.events.slice().reverse().map(e => `<p>${escape(e)}</p>`).join('') : '<p>Событий пока нет.</p>');
}
function renderSettings() {
  contentHTML(`<section class="page narrow" id="settings-page"><div class="page-head"><div><h1>Настройки</h1><p class="page-sub">${escape(appLabel())}</p></div></div>
    <div class="card"><div class="row"><span class="row-label">Тема</span><div class="segmented" role="group" aria-label="Тема"><button id="theme-dark" class="seg ${preferences.theme !== 'light' ? 'active' : ''}" aria-pressed="${preferences.theme !== 'light'}">Тёмная</button><button id="theme-light" class="seg ${preferences.theme === 'light' ? 'active' : ''}" aria-pressed="${preferences.theme === 'light'}">Светлая</button></div></div><label class="row"><span class="row-label">Сворачивать в трей</span><input id="minimize-tray" type="checkbox" class="switch" ${preferences.minimize_to_tray ? 'checked' : ''} /></label></div>
    <h2 class="group-title">Обновления</h2><div class="card"><label class="row"><span class="row-label">Проверять при запуске</span><input id="auto-update" type="checkbox" class="switch" ${preferences.check_updates ? 'checked' : ''} /></label><div class="row"><span class="row-label muted" id="update-message" role="status">${escape(updateMessage)}</span><div class="row-actions"><button class="button" id="check-update" ${updating ? 'disabled' : ''}>${updating ? 'Проверяем…' : 'Проверить'}</button>${updateVersion ? `<button class="button primary" id="install-update" ${updating ? 'disabled' : ''}>Установить ${escape(updateVersion)}</button>` : ''}</div></div></div>
    <h2 class="group-title">Steam</h2><div class="card"><button class="row row-button" id="steam-help"><span class="row-label">Скрыть статус «играет»</span><span class="chev" aria-hidden="true">→</span></button></div>
    <h2 class="group-title">Дополнительно</h2><div class="card"><button class="row row-button" id="diagnostics"><span class="row-label">Диагностика</span><span class="chev" aria-hidden="true">→</span></button><div class="row"><span class="row-label">Завершение работы</span><button class="button" id="quit">Выйти</button></div></div></section>`);
  document.querySelector<HTMLButtonElement>('#theme-dark')!.onclick = () => setTheme('dark');
  document.querySelector<HTMLButtonElement>('#theme-light')!.onclick = () => setTheme('light');
  document.querySelector<HTMLInputElement>('#minimize-tray')!.onchange = e => { void savePreferences({ ...preferences, minimize_to_tray: (e.target as HTMLInputElement).checked }); };
  document.querySelector<HTMLInputElement>('#auto-update')!.onchange = e => { void savePreferences({ ...preferences, check_updates: (e.target as HTMLInputElement).checked }); };
  document.querySelector<HTMLButtonElement>('#diagnostics')!.onclick = () => { page = 'diagnostics'; render(); };
  document.querySelector<HTMLButtonElement>('#quit')!.onclick = () => { void desktop('quit'); };
  document.querySelector<HTMLButtonElement>('#steam-help')!.onclick = () => (document.querySelector('#steam-help-dialog') as HTMLDialogElement).showModal();
  document.querySelector<HTMLButtonElement>('#check-update')!.onclick = () => { void checkUpdate(); };
  const install = document.querySelector<HTMLButtonElement>('#install-update');
  if (install) install.onclick = async () => {
    updating = true; updateMessage = 'Загрузка и проверка подписи.'; renderSettings();
    try { await invoke('install_update'); } catch (error) { updateMessage = `Обновление не установлено: ${String(error)}`; }
    finally { updating = false; updateVersion = null; if (page === 'settings') renderSettings(); }
  };
}
function setTheme(theme: string) { applyTheme(theme); void savePreferences({ ...preferences, theme }); }
async function desktop(command: string, args?: Record<string, unknown>) {
  if (!isTauri()) { notice('Это браузерный просмотр. Настройки и выход доступны в desktop-клиенте.'); return false; }
  try { await invoke(command, args); return true; } catch (error) { notice(String(error)); return false; }
}
async function savePreferences(next: Settings) {
  if (await desktop('save_settings', { settings: next })) preferences = next;
  if (page === 'settings') renderSettings();
}
async function checkUpdate() {
  if (!isTauri()) { notice('Проверка обновлений доступна в desktop-клиенте.'); return; }
  if (updating) return;
  updating = true; updateMessage = 'Проверяем…'; updateVersion = null;
  if (page === 'settings') renderSettings();
  try {
    const result = await invoke<{ configured: boolean; version: string | null }>('check_update');
    updateVersion = result.version;
    updateMessage = !result.configured ? 'Обновления не настроены в этой сборке.' : result.version ? `Доступна версия ${result.version}.` : 'Установлена актуальная версия.';
    if (result.version && page !== 'settings') notice(`Доступна версия ${result.version}.`);
  } catch (error) { updateMessage = `Не удалось проверить обновления: ${String(error)}`; }
  finally { updating = false; if (page === 'settings') renderSettings(); }
}
function openCreate() { setCreateAccess(false); (document.querySelector('#create-dialog') as HTMLDialogElement).showModal(); }
function setCreateAccess(publicAccess: boolean) {
  createPublic = publicAccess;
  document.querySelector('#create-private')!.classList.toggle('active', !publicAccess);
  document.querySelector('#create-public')!.classList.toggle('active', publicAccess);
  document.querySelector('#create-private')!.setAttribute('aria-pressed', String(!publicAccess));
  document.querySelector('#create-public')!.setAttribute('aria-pressed', String(publicAccess));
  document.querySelector<HTMLElement>('#create-password-label')!.hidden = !publicAccess;
  if (!publicAccess) document.querySelector<HTMLInputElement>('#create-password')!.value = '';
}
function openInvite() {
  const active = network(); if (!active) return;
  document.querySelector('#friend-list')!.innerHTML = `${state.friends.length ? state.friends.map(f => `<button class="friend-choice" data-friend="${f.steam_id}"><span>${escape(f.name)}</span><span>Пригласить</span></button>`).join('') : '<p class="muted">Список друзей пуст.</p>'}`;
  document.querySelectorAll<HTMLButtonElement>('[data-friend]').forEach(button => button.onclick = async () => { if (await send({ type: 'invite_friend', network: active.id, steam_id: button.dataset.friend })) (document.querySelector('#invite-dialog') as HTMLDialogElement).close(); });
  (document.querySelector('#invite-dialog') as HTMLDialogElement).showModal();
}
document.querySelector('#create')!.addEventListener('click', openCreate);
document.querySelector('#discover')!.addEventListener('click', () => { page = 'discover'; render(); void send({ type: 'discover' }); });
document.querySelector<HTMLFormElement>('#password-form')!.onsubmit = async e => {
  e.preventDefault();
  const dialog = document.querySelector<HTMLDialogElement>('#password-dialog')!;
  const input = document.querySelector<HTMLInputElement>('#password-input')!;
  if (await send({ type: dialog.dataset.editing === 'true' ? 'set_password' : 'submit_password', network: dialog.dataset.network, password: input.value })) { input.value = ''; dialog.close(); }
};
document.querySelector('#create-private')!.addEventListener('click', () => setCreateAccess(false));
document.querySelector('#create-public')!.addEventListener('click', () => setCreateAccess(true));
document.querySelector('#settings')!.addEventListener('click', () => { page = 'settings'; render(); });
document.querySelector('.brand')!.addEventListener('click', e => { e.preventDefault(); page = 'networks'; render(); });
document.querySelector('#overlay')!.addEventListener('click', () => { if (selected) send({ type: 'invite', network: selected }); });
document.querySelectorAll<HTMLButtonElement>('.close').forEach(button => button.onclick = () => button.closest('dialog')!.close());
document.querySelector('#open-privacy')!.addEventListener('click', () => { void desktop('open_steam_privacy'); });
document.querySelectorAll<HTMLImageElement>('.step-shot').forEach(image => image.addEventListener('error', () => image.classList.add('missing')));
document.querySelector<HTMLFormElement>('#create-form')!.onsubmit = async e => {
  e.preventDefault(); const input = document.querySelector<HTMLInputElement>('#network-name')!;
  const password = document.querySelector<HTMLInputElement>('#create-password')!;
  if (await send({ type: 'create', name: input.value.trim(), public: createPublic, password: createPublic ? password.value : '' })) { (document.querySelector('#create-dialog') as HTMLDialogElement).close(); input.value = ''; password.value = ''; }
};
document.querySelector<HTMLFormElement>('#join-form')!.onsubmit = async e => {
  e.preventDefault(); const input = document.querySelector<HTMLInputElement>('#lobby-id')!;
  if (await send({ type: 'join', lobby: input.value.trim() })) (document.querySelector('#join-dialog') as HTMLDialogElement).close();
};
document.querySelector<HTMLFormElement>('#delete-form')!.onsubmit = async e => {
  e.preventDefault(); const dialog = document.querySelector<HTMLDialogElement>('#delete-dialog')!;
  if (await send({ type: 'delete', network: dialog.dataset.network })) dialog.close();
};

async function poll() {
  if (isTauri()) {
    try {
      const next = await invoke<Snapshot>('snapshot');
      const serialized = JSON.stringify({ steam: next.steam, steam_id: next.steam_id, nickname: next.nickname, relay: next.relay, networks: next.networks, joins: next.joins, public_networks: next.public_networks });
      if (next.events.length && next.events.at(-1) !== state.events.at(-1) && next.events.at(-1)!.startsWith('Ошибка:')) notice(next.events.at(-1)!);
      state = next;
      if (serialized !== previous) {
        previous = serialized;
        if (!selected || !state.networks.some(n => n.id === selected)) selected = state.networks[0]?.id;
        render();
      }
      updateDynamic();
    } catch (error) { notice(`Нет связи с ядром: ${String(error)}`); }
  }
  setTimeout(poll, 1000);
}
applyTheme(preferences.theme);
render();
// The webview's default context menu (back/reload/print/view source) has no
// place in this utility; right click stays inert until custom actions exist.
if (isTauri()) document.addEventListener('contextmenu', e => e.preventDefault());
if (isTauri()) void invoke<string>('version').then(value => { if (value) { setVersion(value); if (page === 'settings') renderSettings(); } }).catch(() => {});
if (isTauri()) void invoke<Settings>('settings').then(result => { if (result) { preferences = result; applyTheme(preferences.theme); } if (page === 'settings') renderSettings(); if (preferences.check_updates) void checkUpdate(); }).catch(error => notice(`Не удалось загрузить настройки: ${String(error)}`));
void poll();
