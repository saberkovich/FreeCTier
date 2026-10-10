import { invoke, isTauri } from '@tauri-apps/api/core';
import morphdom from 'morphdom';
import './fonts';
import './styles.css';

type Peer = { steam_id: string; name: string; ip: string; active: boolean; state: string; ping_ms?: number; can_invite: boolean; may_invite: boolean; can_kick: boolean; may_kick: boolean };
type Network = { id: string; name: string; subnet: string; owner: string; revision: number; adapter: boolean; lobby?: string; public: boolean; can_invite: boolean; password: boolean; members: Peer[] };
type Snapshot = { steam: string; steam_id?: string; nickname?: string; relay: string; networks: Network[]; friends: { steam_id: string; name: string }[]; events: string[]; received: number; sent: number; dropped: number; joins?: { id: string; name: string; password: boolean }[]; public_networks?: { lobby: string; name: string }[] };
type Command = { type: string; [key: string]: unknown };
type Settings = { minimize_to_tray: boolean; check_updates: boolean; autostart?: boolean; theme: string; accent?: string; custom_colors?: Record<string, string> };

let state: Snapshot = { steam: 'waiting', relay: 'unknown', networks: [], friends: [], events: [], received: 0, sent: 0, dropped: 0 };
let selected: string | undefined;
let previous = '';
let page: 'networks' | 'settings' | 'diagnostics' | 'discover' = 'networks';
let preferences: Settings = { minimize_to_tray: true, check_updates: true, autostart: false, theme: 'dark' };
let updateMessage = 'Проверок ещё не было.';
let updateVersion: string | null = null;
let updating = false;
let busy = false;
let appVersion = '';
let createPublic = false;
let discoverQuery = '';
let copied = 0;
const app = document.querySelector<HTMLDivElement>('#app')!;
app.innerHTML = `
  <nav class="rail" aria-label="Разделы">
    <a class="brand" href="#" aria-label="FreeC Tier — мои сети"><span class="brand-mark" aria-hidden="true">F</span><small class="brand-version sr-only" id="brand-version"></small></a>
    <span class="rail-sep" aria-hidden="true"></span>
    <div id="network-list" role="group" aria-label="Сохранённые сети"></div>
    <button class="rail-button add" id="create" title="Создать сеть" aria-label="Создать сеть">+</button>
    <button class="rail-button live" id="discover" title="Публичные сети">LIVE</button>
    <span class="rail-grow"></span>
    <button class="rail-button cog" id="settings" title="Настройки" aria-label="Настройки"><span aria-hidden="true">⚙</span></button>
  </nav>
  <div class="main">
    <header class="topbar"><div class="topbar-title" id="page-title">FreeC Tier</div><span class="topbar-sub" id="page-sub"></span><span class="topbar-grow"></span><button id="header-update" class="header-update" hidden></button><div id="session" role="status"></div></header>
    <main id="content"></main>
  </div>
  <div id="notice" role="status" hidden></div>
  <div id="joins-bar" role="status" hidden></div>
  <dialog id="create-dialog"><form id="create-form"><div class="dialog-heading"><h2>Новая сеть</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p>Подсеть 10.77.x.0/24 назначится автоматически.</p><label for="network-name">Название</label><input id="network-name" name="name" maxlength="64" required placeholder="Например, Minecraft" autocomplete="off" /><label>Доступ</label><div class="segmented wide" role="group" aria-label="Доступ"><button type="button" id="create-private" class="seg active" aria-pressed="true">Приватная</button><button type="button" id="create-public" class="seg" aria-pressed="false">Публичная</button></div><label id="create-password-label" hidden>Пароль (необязательно)<input id="create-password" type="password" maxlength="128" autocomplete="new-password" /></label><p class="hint">В публичной сети участники с разрешением могут приглашать друзей.</p><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button primary" type="submit">Создать сеть</button></div></form></dialog>
  <dialog id="join-dialog"><form id="join-form"><div class="dialog-heading"><h2>Подключиться по lobby</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><label for="lobby-id">Steam Lobby ID</label><input id="lobby-id" name="lobby" required inputmode="numeric" pattern="[0-9]{1,20}" placeholder="64-битный идентификатор" /><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button primary" type="submit">Присоединиться</button></div></form></dialog>
  <dialog id="invite-dialog"><div class="dialog-heading"><h2>Пригласить друга</h2><button class="icon-button close" aria-label="Закрыть">×</button></div><div id="friend-list"></div><button id="overlay" class="button">Открыть список в Steam</button></dialog>
  <dialog id="delete-dialog"><form id="delete-form"><div class="dialog-heading"><h2>Удалить сеть?</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p id="delete-description"></p><p>У остальных участников сеть останется.</p><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button danger" type="submit">Удалить сеть</button></div></form></dialog>
  <dialog id="password-dialog"><form id="password-form"><div class="dialog-heading"><h2 id="password-title">Пароль сети</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><label for="password-input">Пароль</label><input id="password-input" type="password" maxlength="128" autocomplete="current-password" /><p class="hint" id="password-hint"></p><div class="dialog-actions"><button class="button primary" type="submit">Продолжить</button></div></form></dialog>
  <dialog id="steam-help-dialog"><div class="dialog-heading"><h2>Скрыть статус в Steam</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p class="steam-help-note">Приложение работает через Steam, поэтому Steam показывает запущенную игру. Скройте её в своей библиотеке:</p><div class="steam-help">
    <section class="step"><span class="step-num">1</span><div class="step-body"><h3>Скрыть игру в библиотеке</h3><p>Библиотека → правый клик по игре → «Управление» → «Сделать приватной».</p><img class="step-shot" src="/steam/library.png" alt="Контекстное меню игры в библиотеке Steam" loading="lazy" /></div></section>
  </div></dialog>`;

const content = document.querySelector<HTMLElement>('#content')!;
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
function network() { return state.networks.find(n => n.id === selected); }
function appLabel() { return appVersion ? `FreeC Tier ${appVersion}` : 'FreeC Tier'; }
// Identity colors: avatars differ by name so the rail stays scannable, while
// saturation and lightness stay with the theme.
function hue(value: string) { let total = 0; for (const point of value || '?') total = (total * 31 + point.codePointAt(0)!) % 360; return total; }
function initials(value: string) { return escape([...(value || '?')].slice(0, 2).join('').toUpperCase()); }
function quality(ping?: number) { return ping == null ? 'none' : ping < 40 ? 'good' : ping < 80 ? 'ok' : 'bad'; }
function online(peer: Peer) { return peer.active && peer.state !== 'offline' && peer.state !== 'removed'; }

function applyTheme(theme: string) {
  const root = document.documentElement;
  for (const token of THEME_TOKENS) root.style.removeProperty(`--${token}`);
  // No `custom` block in CSS: the :root defaults act as the base and the
  // user's palette is layered on top as inline variables.
  root.dataset.theme = theme === 'custom' ? 'custom' : theme === 'light' ? 'light' : 'dark';
  // Read the preset accent with every override stripped, so the stylesheet
  // stays the only place where a theme's own accent is written down.
  presetAccent = getComputedStyle(root).getPropertyValue('--accent').trim();
  if (theme === 'custom') {
    for (const [token, value] of Object.entries(preferences.custom_colors ?? {})) {
      if (value) root.style.setProperty(`--${token}`, value);
    }
  } else if (preferences.accent) {
    applyAccent(preferences.accent);
  }
}
// The swatch sets one color; the shades around it follow the active theme, so
// one accent works on both the dark and the light surface.
function applyAccent(color: string) {
  const root = document.documentElement.style;
  root.setProperty('--accent', color);
  root.setProperty('--accent-hover', `color-mix(in oklch, ${color} 82%, var(--text))`);
  root.setProperty('--accent-soft', `color-mix(in oklch, ${color} 20%, var(--bg))`);
  root.setProperty('--focus', `color-mix(in oklch, ${color} 72%, var(--text))`);
}
function setAccent(color?: string) {
  preferences = { ...preferences, accent: color };
  applyTheme(preferences.theme);
  void savePreferences(preferences);
}
let presetAccent = '';
// Three alternatives next to whatever the current theme calls its own accent.
const ACCENTS: Array<[string, string]> = [['#8b5cf6', 'Фиолетовый'], ['#22c1d6', 'Бирюзовый'], ['#3ecf8e', 'Зелёный']];
const THEME_TOKENS = ['accent', 'accent-hover', 'accent-soft', 'focus', 'text', 'muted', 'faint', 'bg', 'surface', 'surface-2', 'surface-3', 'line', 'line-soft', 'success', 'warning', 'danger'] as const;
const THEME_LABELS: Record<string, string> = {
  accent: 'Акцент', 'accent-hover': 'Акцент (наведение)', 'accent-soft': 'Акцент (подложка)', focus: 'Обводка фокуса',
  text: 'Текст', muted: 'Приглушённый текст', faint: 'Слабый текст', bg: 'Фон',
  surface: 'Поверхность', 'surface-2': 'Панель слева', 'surface-3': 'Поверхность 3',
  line: 'Линии', 'line-soft': 'Тонкие линии',
  success: 'Успех', warning: 'Предупреждение', danger: 'Опасность',
};
const THEME_GROUPS: Array<[string, string[]]> = [
  ['Основные цвета', ['bg', 'surface', 'surface-2', 'surface-3', 'text', 'muted', 'faint', 'line', 'line-soft']],
  ['Акцент', ['accent', 'accent-hover', 'accent-soft', 'focus']],
  ['Статусы', ['success', 'warning', 'danger']],
];
function setVersion(value: string) {
  appVersion = value;
  const brand = document.querySelector('#brand-version'); if (brand) brand.textContent = value;
  document.querySelector('.brand')!.setAttribute('title', appLabel());
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
  renderHeaderUpdate();
  const connected = state.steam === 'online';
  patch(document.querySelector<HTMLElement>('#session')!, `<span class="steam-pill ${connected ? 'online' : 'offline'}"><i aria-hidden="true"></i>${connected ? 'Steam подключён' : 'Steam не подключён'}</span>${state.nickname ? `<span class="account"><span class="account-face" aria-hidden="true" style="--h:${hue(state.nickname)}">${initials(state.nickname)}</span><span class="account-name">${escape(state.nickname)}</span></span>` : ''}`);
  patch(document.querySelector<HTMLElement>('#network-list')!, state.networks.map(n => {
    const current = n.id === selected && page === 'networks';
    return `<button id="network-${n.id}" class="rail-net ${current ? 'selected' : ''}" data-select="${n.id}" title="${escape(n.name)}" aria-label="${escape(n.name)}" ${current ? 'aria-current="page"' : ''} style="--h:${hue(n.name)}"><span class="rail-pill" aria-hidden="true"></span><span class="rail-avatar" aria-hidden="true">${initials(n.name)}</span><span class="rail-dot ${n.adapter ? 'on' : ''}" aria-hidden="true"></span></button>`;
  }).join(''));
  document.querySelectorAll<HTMLButtonElement>('[data-select]').forEach(button => button.onclick = () => { selected = button.dataset.select; page = 'networks'; render(); });
  document.querySelector('#discover')!.classList.toggle('selected', page === 'discover');
  document.querySelector('#settings')!.classList.toggle('selected', page === 'settings' || page === 'diagnostics');
  if (page === 'diagnostics') { renderDiagnostics(); return; }
  if (page === 'settings') { renderSettings(); return; }
  if (page === 'discover') { renderDiscovery(); return; }
  const active = network();
  if (!active) {
    heading('Мои сети', 'Пока ни одной сети');
    contentHTML(`<section class="empty-state" id="empty"><svg class="empty-art" viewBox="0 0 180 110" role="img" aria-hidden="true"><path d="M42 48 L90 60 M138 48 L90 60" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" /><rect x="20" y="14" width="44" height="34" rx="9" fill="none" stroke="currentColor" stroke-width="2" /><rect x="116" y="14" width="44" height="34" rx="9" fill="none" stroke="currentColor" stroke-width="2" /><rect x="66" y="60" width="48" height="38" rx="10" fill="var(--accent)" /></svg><h1>Объедините компьютеры в одну сеть</h1><p>Как в одной локальной сети — для игр и любых LAN-приложений.</p><button class="button primary" id="create-first">Создать первую сеть</button></section>`);
    document.querySelector<HTMLButtonElement>('#create-first')!.onclick = openCreate;
    return;
  }
  heading(active.name, `Виртуальная сеть · ${active.members.length} участн.`);
  renderNetwork(active, connected);
}
function heading(title: string, sub: string) {
  document.querySelector('#page-title')!.textContent = title;
  document.querySelector('#page-sub')!.textContent = sub;
}

function renderNetwork(active: Network, connected: boolean) {
  const owner = active.owner === state.steam_id;
  const local = active.members.find(m => m.steam_id === state.steam_id);
  const on = active.adapter;
  const present = active.members.filter(online);
  const away = active.members.filter(m => !online(m));
  const pings = present.map(m => m.ping_ms).filter((p): p is number => p != null);
  const average = pings.length ? Math.round(pings.reduce((a, b) => a + b, 0) / pings.length) : '—';
  const statusText = !local?.active ? 'Доступ к сети отозван владельцем'
    : on ? 'Сеть включена · вы в одной LAN'
    : 'Сеть выключена · нажмите, чтобы включить';
  contentHTML(`<section class="page net-page" id="detail-${active.id}">
    <section class="hero ${on ? 'on' : ''}">
      <div class="hero-power">${on ? '<span class="hero-ring" aria-hidden="true"></span>' : ''}<button id="toggle-adapter" class="power" aria-label="${on ? 'Выключить' : 'Включить'}" title="${on ? 'Выключить' : 'Включить'}" ${!connected || !local?.active ? 'disabled' : ''}><svg width="44" height="44" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M12 3v8M6.3 6.3a8 8 0 1 0 11.4 0" /></svg></button></div>
      <div class="hero-body">
        <div class="hero-name"><h1>${escape(active.name)}</h1><span class="tag">${active.public ? 'Публичная' : 'Приватная'}</span></div>
        <p class="hero-status">${statusText}</p>
        <div class="hero-actions">${local?.active ? `<button id="copy-ip" class="ip-chip" title="Скопировать адрес"><span class="ip-label">ВАШ IP</span><code>${escape(local.ip)}</code><span class="ip-copy">${copied === 1 ? 'СКОПИРОВАНО' : 'КОПИРОВАТЬ'}</span></button>` : ''}<button id="invite" class="button primary" ${!active.can_invite || !connected ? 'disabled' : ''}>+ Пригласить друга</button></div>
      </div>
      <div class="hero-stats"><div><strong>${present.length}<small>/${active.members.length}</small></strong><span>в сети</span></div><div><strong>${average}<small> мс</small></strong><span>средний пинг</span></div></div>
    </section>
    ${!connected ? '<p class="hint warning">Steam не подключён — включение сети и приглашения недоступны. Причина в журнале событий на вкладке «Диагностика».</p>' : ''}
    <div class="net-grid">
      <section class="members" aria-label="Участники">
        <h2 class="members-head">В СЕТИ — ${present.length}</h2>
        ${present.map(peer => member(active, peer, owner, connected)).join('') || '<p class="hint">Пока никого. Пригласите друга из Steam.</p>'}
        ${away.length ? `<h2 class="members-head">НЕ В СЕТИ — ${away.length}</h2>${away.map(peer => member(active, peer, owner, connected)).join('')}` : ''}
      </section>
      <aside class="net-side">
        <div class="panel"><div class="panel-head">ДОСТУП</div>
          ${owner
            ? `<div class="segmented wide" role="group" aria-label="Доступ"><button id="access-private" class="seg ${active.public ? '' : 'active'}" aria-pressed="${!active.public}" ${connected ? '' : 'disabled'}>Приватная</button><button id="access-public" class="seg ${active.public ? 'active' : ''}" aria-pressed="${active.public}" ${connected ? '' : 'disabled'}>Публичная</button></div>`
            : `<p class="row-label">${active.public ? 'Публичная' : 'Приватная'}</p>`}
          <p class="hint">${active.public ? 'Сеть видна в списке публичных. Участники с правом могут приглашать друзей.' : 'Вход только по приглашению владельца или участников с правом приглашать.'}</p>
          ${active.public && owner ? `<button class="button wide" id="set-password" ${connected ? '' : 'disabled'}>Пароль: ${active.password ? 'задан' : 'не задан'} · ${active.password ? 'Изменить' : 'Установить'}</button>` : ''}
        </div>
        <div class="panel"><div class="panel-head">ТРАФИК · ПАКЕТЫ</div><div class="meter"><span>↑ Отправлено</span><b id="traffic-sent">${state.sent}</b></div><div class="meter"><span>↓ Получено</span><b id="traffic-received">${state.received}</b></div></div>
        <button class="side-link" id="go-diagnostics">Диагностика <span aria-hidden="true">→</span></button>
        <button class="text-button danger" id="delete-network">Удалить сеть с компьютера</button>
      </aside>
    </div></section>`);
  document.querySelector<HTMLButtonElement>('#invite')!.onclick = openInvite;
  document.querySelector<HTMLButtonElement>('#toggle-adapter')!.onclick = () => { void send({ type: 'set_adapter', network: active.id, enabled: !active.adapter }); };
  document.querySelector<HTMLButtonElement>('#go-diagnostics')!.onclick = () => { page = 'diagnostics'; render(); };
  document.querySelector<HTMLButtonElement>('#copy-ip')?.addEventListener('click', () => { void copyAddress(local!.ip); });
  const privateAccess = document.querySelector<HTMLButtonElement>('#access-private');
  const publicAccess = document.querySelector<HTMLButtonElement>('#access-public');
  if (privateAccess) privateAccess.onclick = () => { void send({ type: 'set_access', network: active.id, public: false }); };
  if (publicAccess) publicAccess.onclick = () => { void send({ type: 'set_access', network: active.id, public: true }); };
  document.querySelectorAll<HTMLInputElement>('[data-permission]').forEach(input => input.onchange = () => {
    // Read both checkboxes from the row, not the render-time snapshot: a quick
    // second toggle must not resurrect the stale value of the first one.
    const row = input.closest('.member-perms')!;
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
// One row shape for both groups: an offline member still needs owner controls,
// otherwise rights could only be granted while the friend happens to be up.
function member(active: Network, peer: Peer, owner: boolean, connected: boolean) {
  const here = online(peer);
  const name = peer.name || peer.steam_id;
  const role: string[] = [];
  if (peer.steam_id === active.owner) role.push('ВЛАДЕЛЕЦ');
  else if (peer.may_invite) role.push('МОЖЕТ ПРИГЛАШАТЬ');
  if (peer.steam_id === state.steam_id) role.push('ВЫ');
  const lit = peer.ping_ms == null ? 4 : peer.ping_ms < 40 ? 4 : peer.ping_ms < 80 ? 3 : 2;
  const manage = owner && peer.steam_id !== active.owner;
  const removable = manage || (peer.may_kick && peer.active);
  return `<div class="member ${here ? '' : 'off'}" id="peer-${active.id}-${peer.steam_id}" data-q="${here ? quality(peer.ping_ms) : 'none'}" style="--h:${hue(name)}">
    <div class="member-face"><span class="avatar" aria-hidden="true">${initials(name)}</span><span class="member-dot ${here ? escape(peer.state) : ''}" aria-hidden="true"></span></div>
    <div class="member-id"><div class="member-name"><strong>${escape(name)}</strong>${role.length ? `<span class="role ${peer.steam_id === active.owner ? 'owner' : ''}">${role.join(' · ')}</span>` : ''}</div><code>${escape(peer.ip)}</code></div>
    ${manage ? `<div class="member-perms"><label class="chip"><input type="checkbox" data-permission="invite" data-peer="${peer.steam_id}" aria-label="Приглашать: ${escape(name)}" ${peer.can_invite ? 'checked' : ''} ${connected ? '' : 'disabled'} />Приглашать</label><label class="chip"><input type="checkbox" data-permission="kick" data-peer="${peer.steam_id}" aria-label="Исключать: ${escape(name)}" ${peer.can_kick ? 'checked' : ''} ${connected ? '' : 'disabled'} />Исключать</label></div>` : ''}
    ${here
      ? `<div class="bars" role="img" aria-label="${escape(labels[peer.state] || peer.state)}">${[0, 1, 2, 3].map(i => `<i class="${i < lit ? 'on' : ''}"></i>`).join('')}</div><div class="member-ping">${peer.ping_ms == null ? '—' : `${peer.ping_ms} мс`}</div>`
      : `<span class="member-state">${escape(peer.active ? labels[peer.state] || peer.state : 'Доступ отозван')}</span>`}
    ${removable ? `<button class="text-button danger" data-member="${peer.steam_id}" data-active="${peer.active}" ${connected ? '' : 'disabled'}>${peer.active ? 'Удалить' : 'Вернуть'}</button>` : ''}
  </div>`;
}
async function copyAddress(ip: string) {
  try { await navigator.clipboard.writeText(ip); } catch { notice(`Адрес не скопирован: ${ip}`); return; }
  copied = 1; render();
  setTimeout(() => { copied = 0; render(); }, 1600);
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
// An available update is always one click away in the header, on any page.
function renderHeaderUpdate() {
  const chip = document.querySelector<HTMLButtonElement>('#header-update')!;
  if (!updateVersion) { chip.hidden = true; return; }
  chip.hidden = false;
  chip.disabled = updating;
  chip.textContent = updating ? 'Обновляем…' : `Обновить до ${updateVersion}`;
  chip.onclick = () => { void installUpdate(); };
}
async function installUpdate() {
  updating = true; updateMessage = 'Загрузка и проверка подписи.';
  renderHeaderUpdate();
  if (page === 'settings') renderSettings();
  try { await invoke('install_update'); } catch (error) { updateMessage = `Обновление не установлено: ${String(error)}`; notice(updateMessage); }
  finally { updating = false; updateVersion = null; renderHeaderUpdate(); if (page === 'settings') renderSettings(); }
}
function renderDiscovery() {
  const joins = state.joins || [];
  const query = discoverQuery.trim().toLowerCase();
  const found = (state.public_networks || []).filter(n => !query || n.name.toLowerCase().includes(query));
  heading('Публичные сети', `Найдено ${found.length}`);
  contentHTML(`<section class="page" id="discovery-page">
    <div class="page-head"><div><h1>Публичные сети</h1><p class="page-sub">Открытые сети, которые сейчас видит Steam</p></div><div class="page-actions"><input id="discover-search" class="search" placeholder="Поиск по названию" aria-label="Поиск по названию" autocomplete="off" /><button class="button" id="refresh-public" ${state.steam === 'online' ? '' : 'disabled'}>Обновить</button></div></div>
    ${joins.map(j => `<div class="card" id="pending-${j.id}"><div class="row"><span class="row-label">${escape(j.name)}<small>${j.password ? 'Требуется пароль' : 'Подключение…'}</small></span><div class="row-actions">${j.password ? `<button class="button primary" data-password="${j.id}">Ввести пароль</button>` : ''}<button class="text-button" data-cancel-join="${j.id}">Отмена</button></div></div></div>`).join('')}
    ${found.length
      ? `<div class="discover-grid">${found.map(n => `<div class="pub-card"><div class="pub-head"><span class="avatar" aria-hidden="true" style="--h:${hue(n.name)}">${initials(n.name)}</span><div style="min-width:0"><b>${escape(n.name)}</b><small>Публичная сеть</small></div></div><button class="button primary" data-lobby="${escape(n.lobby)}">Войти</button></div>`).join('')}</div>`
      : `<div class="card"><p class="hint">${query ? 'Ничего не найдено по этому запросу.' : 'Сети не найдены. Нажмите «Обновить», чтобы повторить поиск.'}</p></div>`}
    </section>`);
  document.querySelector<HTMLButtonElement>('#refresh-public')!.onclick = () => { void send({ type: 'discover' }); };
  document.querySelector<HTMLInputElement>('#discover-search')!.oninput = event => { discoverQuery = (event.target as HTMLInputElement).value; renderDiscovery(); };
  document.querySelectorAll<HTMLButtonElement>('[data-lobby]').forEach(b => b.onclick = () => { void send({ type: 'join', lobby: b.dataset.lobby }); });
  document.querySelectorAll<HTMLButtonElement>('[data-password]').forEach(b => b.onclick = () => openPassword(b.dataset.password!, false));
  document.querySelectorAll<HTMLButtonElement>('[data-cancel-join]').forEach(b => b.onclick = () => { void send({ type: 'cancel_join', network: b.dataset.cancelJoin }); });
}

function renderDiagnostics() {
  heading('Диагностика', 'Состояние клиента');
  contentHTML(`<section class="page narrow" id="diagnostic-page"><button id="back-settings" class="text-button back">← Настройки</button><div class="page-head"><div><h1>Диагностика</h1><p class="page-sub">Состояние клиента и события</p></div></div><div class="panel"><dl class="dl"><dt>SteamID</dt><dd><code>${escape(state.steam_id || 'Не получен')}</code></dd><dt>Steam Relay</dt><dd>${escape(state.relay)}</dd><dt>Транспорт</dt><dd>Steam P2P · ICE отключён</dd><dt>Адаптер</dt><dd>Wintun · IPv4 · MTU 1280</dd><dt>Среда</dt><dd>${isTauri() ? 'Desktop-клиент' : 'Браузерный просмотр — Steam недоступен'}</dd></dl></div><p class="counters" id="counters"></p>${state.networks.map(n => `<details class="net" id="info-${n.id}"><summary>${escape(n.name)}</summary><dl class="dl"><dt>Network ID</dt><dd><code>${n.id}</code></dd><dt>Подсеть</dt><dd>${escape(n.subnet)}</dd><dt>Версия</dt><dd>${n.revision}</dd><dt>Lobby ID</dt><dd><code>${n.lobby || '—'}</code></dd></dl></details>`).join('')}<div class="section-head"><h2>Журнал событий</h2></div><div class="event-log" id="event-log"></div><div class="page-actions"><button id="join" class="button">Войти по Steam Lobby ID</button></div></section>`);
  document.querySelector<HTMLButtonElement>('#back-settings')!.onclick = () => { page = 'settings'; render(); };
  document.querySelector<HTMLButtonElement>('#join')!.onclick = () => document.querySelector<HTMLDialogElement>('#join-dialog')!.showModal();
  updateDynamic();
}
function updateDynamic() {
  const counters = document.querySelector('#counters');
  if (counters) counters.textContent = `Отправлено ${state.sent} · Получено ${state.received} · Отклонено ${state.dropped}`;
  const sent = document.querySelector('#traffic-sent');
  if (sent) { sent.textContent = String(state.sent); document.querySelector('#traffic-received')!.textContent = String(state.received); }
  const log = document.querySelector<HTMLElement>('#event-log');
  if (log) patch(log, state.events.length ? state.events.slice().reverse().map(e => `<p>${escape(e)}</p>`).join('') : '<p>Событий пока нет.</p>');
}
function renderSettings() {
  heading('Настройки', appLabel());
  contentHTML(`<section class="page narrow" id="settings-page"><div class="page-head"><div><h1>Настройки</h1><p class="page-sub">Поведение клиента, служба и обновления</p></div></div>
    <div class="card"><div class="row"><span class="row-label">Тема</span><div class="segmented" role="group" aria-label="Тема"><button id="theme-dark" class="seg ${preferences.theme === 'dark' ? 'active' : ''}" aria-pressed="${preferences.theme === 'dark'}">Тёмная</button><button id="theme-light" class="seg ${preferences.theme === 'light' ? 'active' : ''}" aria-pressed="${preferences.theme === 'light'}">Светлая</button><button id="theme-custom" class="seg ${preferences.theme === 'custom' ? 'active' : ''}" aria-pressed="${preferences.theme === 'custom'}">Своя</button></div></div>${preferences.theme === 'custom' ? '' : `<div class="row"><span class="row-label">Акцентный цвет<small>Главный цвет кнопок и выделений</small></span><div class="swatches">${accentSwatches()}</div></div>`}<label class="row"><span class="row-label">Сворачивать в трей</span><input id="minimize-tray" type="checkbox" class="switch" ${preferences.minimize_to_tray ? 'checked' : ''} /></label><label class="row"><span class="row-label">Запускать при входе в Windows</span><input id="autostart" type="checkbox" class="switch" ${preferences.autostart ? "checked" : ""} /></label></div>
    ${preferences.theme === 'custom' ? `<div class="card" id="theme-editor">${THEME_GROUPS.map(([title, tokens]) => `<h3 class="editor-title">${escape(title)}</h3>${tokens.map(token => `<label class="row color-row"><span class="row-label">${escape(THEME_LABELS[token] ?? token)}</span><input type="color" class="color-input" data-token="${token}" value="${escape(preferences.custom_colors?.[token] ?? '#000000')}" aria-label="${escape(THEME_LABELS[token] ?? token)}" /></label>`).join('')}`).join('')}<div class="row"><span class="row-label muted">Изменения применяются сразу.</span><button class="button" id="theme-reset">Сбросить</button></div></div>` : ''}
    <h2 class="group-title">Служба</h2><div class="card"><div class="row"><span class="row-label">Служба FreeC Tier<small id="service-status">Проверяем…</small></span><div class="row-actions"><button class="button" id="service-install">Переустановить</button><button class="button" id="service-uninstall">Удалить</button></div></div></div>
    <h2 class="group-title">Обновления</h2><div class="card"><label class="row"><span class="row-label">Проверять при запуске</span><input id="auto-update" type="checkbox" class="switch" ${preferences.check_updates ? 'checked' : ''} /></label><div class="row"><span class="row-label muted" id="update-message" role="status">${escape(updateMessage)}</span><div class="row-actions"><button class="button" id="check-update" ${updating ? 'disabled' : ''}>${updating ? 'Проверяем…' : 'Проверить'}</button>${updateVersion ? `<button class="button primary" id="install-update" ${updating ? 'disabled' : ''}>Установить ${escape(updateVersion)}</button>` : ''}</div></div></div>
    <h2 class="group-title">Steam</h2><div class="card"><button class="row row-button" id="steam-help"><span class="row-label">Скрыть статус «играет»</span><span class="chev" aria-hidden="true">→</span></button></div>
    <h2 class="group-title">Дополнительно</h2><div class="card"><button class="row row-button" id="diagnostics"><span class="row-label">Диагностика</span><span class="chev" aria-hidden="true">→</span></button><div class="row"><span class="row-label">Завершение работы</span><button class="button" id="quit">Выйти</button></div></div></section>`);
  document.querySelector<HTMLButtonElement>('#theme-dark')!.onclick = () => setTheme('dark');
  document.querySelector<HTMLButtonElement>('#theme-light')!.onclick = () => setTheme('light');
  document.querySelector<HTMLButtonElement>('#theme-custom')!.onclick = () => setTheme('custom');
  document.querySelectorAll<HTMLButtonElement>('[data-accent]').forEach(button => button.onclick = () => setAccent(button.dataset.accent || undefined));
  document.querySelectorAll<HTMLInputElement>('.color-input').forEach(input => {
    const token = input.dataset.token!;
    input.oninput = () => {
      // Live preview only: persisting on every drag step would spam the
      // settings file, so the value lands on disk when the picker closes.
      preferences.custom_colors = { ...(preferences.custom_colors ?? {}), [token]: input.value };
      applyTheme('custom');
    };
    input.onchange = () => { void savePreferences({ ...preferences, theme: 'custom' }); };
  });
  document.querySelector<HTMLButtonElement>('#theme-reset')?.addEventListener('click', () => {
    applyTheme('dark');
    void savePreferences({ ...preferences, theme: 'dark', custom_colors: undefined });
  });
  document.querySelector<HTMLInputElement>('#minimize-tray')!.onchange = e => { void savePreferences({ ...preferences, minimize_to_tray: (e.target as HTMLInputElement).checked }); };
    async function refreshServiceStatus() {
    if (!isTauri()) return;
    try {
      const status = await invoke<{ installed: boolean; running: boolean }>('service_status');
      const label = document.querySelector('#service-status');
      if (label) label.textContent = status.running ? 'Установлена и запущена' : status.installed ? 'Установлена, но не запущена' : 'Не установлена';
    } catch { /* service missing from this build */ }
  }
  void refreshServiceStatus();
  document.querySelector<HTMLButtonElement>('#service-install')!.onclick = async () => {
    if (await desktop('install_service')) setTimeout(() => void refreshServiceStatus(), 2500);
  };
  document.querySelector<HTMLButtonElement>('#service-uninstall')!.onclick = async () => {
    if (await desktop('uninstall_service')) setTimeout(() => void refreshServiceStatus(), 2500);
  };

  document.querySelector<HTMLInputElement>('#autostart')!.onchange = async e => {
    const input = e.target as HTMLInputElement;
    // The scheduled task is the source of truth: persist the preference only
    // after the task was created or removed, and revert on failure.
    if (await desktop('set_autostart', { enabled: input.checked })) void savePreferences({ ...preferences, autostart: input.checked });
    else input.checked = !input.checked;
  };
  document.querySelector<HTMLInputElement>('#auto-update')!.onchange = e => { void savePreferences({ ...preferences, check_updates: (e.target as HTMLInputElement).checked }); };
  document.querySelector<HTMLButtonElement>('#diagnostics')!.onclick = () => { page = 'diagnostics'; render(); };
  document.querySelector<HTMLButtonElement>('#quit')!.onclick = () => { void desktop('quit'); };
  document.querySelector<HTMLButtonElement>('#steam-help')!.onclick = () => (document.querySelector('#steam-help-dialog') as HTMLDialogElement).showModal();
  document.querySelector<HTMLButtonElement>('#check-update')!.onclick = () => { void checkUpdate(); };
  const install = document.querySelector<HTMLButtonElement>('#install-update');
  if (install) install.onclick = () => { void installUpdate(); };
}
function accentSwatches() {
  const current = (preferences.accent || presetAccent).toLowerCase();
  // An empty `data-accent` means «follow the theme», so switching dark ↔ light
  // keeps each preset's own accent instead of pinning yesterday's color.
  const swatches: Array<[string, string, string]> = [['', presetAccent, 'По умолчанию'], ...ACCENTS.map(([color, label]): [string, string, string] => [color, color, label])];
  return swatches.map(([value, color, label]) => {
    const active = color.toLowerCase() === current;
    return `<button class="swatch ${active ? 'active' : ''}" data-accent="${escape(value)}" style="--c:${escape(color)}" aria-label="Акцент: ${escape(label)}" aria-pressed="${active}"></button>`;
  }).join('');
}
function setTheme(theme: string) {
  if (theme === 'custom' && !preferences.custom_colors) {
    // Seed the custom palette from whatever the user sees right now, so the
    // editor starts from their current theme instead of a blank slate.
    const style = getComputedStyle(document.documentElement);
    const seeded: Record<string, string> = {};
    for (const token of THEME_TOKENS) {
      const value = style.getPropertyValue(`--${token}`).trim();
      if (value) seeded[token] = value;
    }
    preferences = { ...preferences, theme: 'custom', custom_colors: seeded };
  } else {
    preferences = { ...preferences, theme };
  }
  applyTheme(preferences.theme);
  void savePreferences(preferences);
}
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
  finally { updating = false; if (page === 'settings') renderSettings(); renderHeaderUpdate(); }
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
