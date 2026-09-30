import { invoke, isTauri } from '@tauri-apps/api/core';
import morphdom from 'morphdom';
import './styles.css';

type Peer = { steam_id: string; name: string; ip: string; active: boolean; state: string; ping_ms?: number };
type Network = { id: string; name: string; subnet: string; owner: string; revision: number; adapter: boolean; lobby?: string; members: Peer[] };
type Snapshot = { steam: string; steam_id?: string; nickname?: string; relay: string; networks: Network[]; friends: { steam_id: string; name: string }[]; events: string[]; received: number; sent: number; dropped: number };
type Command = { type: string; [key: string]: unknown };

let state: Snapshot = { steam: 'waiting', relay: 'unknown', networks: [], friends: [], events: [], received: 0, sent: 0, dropped: 0 };
let selected: string | undefined;
let previous = '';
let page: 'networks' | 'settings' | 'diagnostics' = 'networks';
type Settings = { minimize_to_tray: boolean; check_updates: boolean };
let preferences: Settings = { minimize_to_tray: true, check_updates: false };
let updateMessage = 'Проверка обновлений выполняется через GitHub Releases.';
let updateVersion: string | null = null;
let updating = false;
let busy = false;
let appVersion = '';
const app = document.querySelector<HTMLDivElement>('#app')!;
app.innerHTML = `
  <header class="topbar"><a class="brand" href="#" aria-label="FreeC Tier — мои сети"><span class="brand-mark">F</span> FreeC Tier <small id="brand-version"></small></a><div id="session" role="status"></div></header>
  <div class="workspace"><aside class="sidebar"><div class="rail-heading"><h2>Мои сети</h2><span id="network-count">0</span></div><nav id="network-list" aria-label="Сохранённые сети"></nav>
  <button class="button primary create" id="create">＋ Создать сеть</button>
  <div class="sidebar-bottom"><button class="button subtle" id="settings">Настройки <span>⚙</span></button><p>Друзья рядом.<br />Даже из другого города.</p></div></aside>
  <main id="content"></main></div><footer class="statusbar"><span id="build-label">FreeC Tier</span><span>Закрытие окна сворачивает приложение в трей</span></footer>
  <div id="notice" role="status" hidden></div>
  <dialog id="create-dialog"><form id="create-form"><div class="dialog-heading"><h2>Новая сеть</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p>Пригласите друзей через Steam. Каждый получит постоянный виртуальный IP.</p><label for="network-name">Название сети</label><input id="network-name" name="name" maxlength="64" required placeholder="Например, Minecraft" autocomplete="off" /><label>Подсеть</label><div class="readonly">Автоматически <span>10.77.x.0/24</span></div><p class="helper">Выбирается свободная подсеть среди сохранённых сетей. Создание не включает адаптер.</p><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button primary" type="submit">Создать сеть</button></div></form></dialog>
  <dialog id="join-dialog"><form id="join-form"><div class="dialog-heading"><h2>Подключиться к сети</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p>Обычно достаточно принять приглашение в Steam. Для диагностики можно указать ID существующего lobby.</p><label for="lobby-id">Steam Lobby ID</label><input id="lobby-id" name="lobby" required inputmode="numeric" pattern="[0-9]{1,20}" placeholder="64-битный Steam Lobby ID" /><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button primary" type="submit">Присоединиться</button></div></form></dialog>
  <dialog id="invite-dialog"><div class="dialog-heading"><h2>Пригласить друга</h2><button class="icon-button close" aria-label="Закрыть">×</button></div><p>Приглашение отправляется через Steam. Новые участники получают доступ к этой виртуальной сети.</p><div id="friend-list"></div><button id="overlay" class="button">Открыть приглашения Steam Overlay</button></dialog>
  <dialog id="delete-dialog"><form id="delete-form"><div class="dialog-heading"><h2>Удалить сеть с компьютера?</h2><button class="icon-button close" type="button" aria-label="Закрыть">×</button></div><p id="delete-description"></p><p>Адаптер будет выключен. Сеть продолжит работать у остальных участников. Для возвращения потребуется приглашение владельца.</p><div class="dialog-actions"><button type="button" class="button close">Отмена</button><button class="button danger" type="submit">Удалить сеть</button></div></form></dialog>`;

const content = document.querySelector<HTMLElement>('#content')!;
// Keyed DOM reconciliation keeps focus, open details and scroll containers alive.
function patch(element: HTMLElement, html: string) {
  const next = element.cloneNode(false) as HTMLElement; next.innerHTML = html;
  morphdom(element, next, { childrenOnly: true, onBeforeElUpdated(from, to) {
    if (from instanceof HTMLDetailsElement) to.toggleAttribute('open', from.open);
    // Dynamic diagnostic children are updated separately; don't clear a
    // scrolling log just because the surrounding network data changed.
    if (from.id === 'event-log' || from.id === 'counters') return false;
    if (from instanceof HTMLInputElement) return true;
    return !from.isEqualNode(to);
  } });
}
function contentHTML(html: string) { patch(content, html); }
function escape(value: string) { return value.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]!)); }
const labels: Record<string, string> = { connected: 'Подключён', connecting: 'Подключение…', reconnecting: 'Переподключение…', offline: 'Не в сети', local: 'Этот компьютер', removed: 'Доступ отозван' };
function status(value: string) { return `<span class="peer-status ${escape(value)}"><i aria-hidden="true"></i>${escape(labels[value] || value)}</span>`; }
function network() { return state.networks.find(n => n.id === selected); }
function appLabel() { return appVersion ? `FreeC Tier ${appVersion}` : 'FreeC Tier'; }
function setVersion(value: string) {
  appVersion = value;
  const brand = document.querySelector('#brand-version'); if (brand) brand.textContent = value;
  const label = document.querySelector('#build-label'); if (label) label.textContent = appLabel();
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
  const online = state.steam === 'online';
  patch(document.querySelector<HTMLElement>('#session')!, `<span class="steam-status ${online ? 'online' : ''}"><i></i>${online ? 'Steam подключён' : 'Ожидание Steam'}</span><span class="account">${escape(state.nickname || 'Войдите в Steam')}</span>`);
  document.querySelector('#network-count')!.textContent = String(state.networks.length);
  patch(document.querySelector<HTMLElement>('#network-list')!, state.networks.length ? state.networks.map(n => `<button id="network-${n.id}" class="network-item ${n.id === selected && page === 'networks' ? 'selected' : ''}" data-select="${n.id}" ${n.id === selected && page === 'networks' ? 'aria-current="page"' : ''}><span class="network-icon ${n.adapter ? 'active' : ''}" aria-hidden="true">⌘</span><span><strong>${escape(n.name)}</strong><small>${n.adapter ? 'Включена' : 'Выключена'}</small></span></button>`).join('') : '<p class="rail-empty">Сохранённые сети появятся здесь.</p>');
  document.querySelectorAll<HTMLButtonElement>('[data-select]').forEach(button => button.onclick = () => { selected = button.dataset.select; page = 'networks'; render(); });
  if (page === 'diagnostics') { renderDiagnostics(); return; }
  if (page === 'settings') { renderSettings(); return; }
  const active = network();
  if (!active) {
    contentHTML(`<section class="empty-state" id="empty"><div class="empty-symbol" aria-hidden="true"><span></span><span></span><span></span></div><h1>Своя локальная сеть.<br /><span>Где бы ни были друзья.</span></h1><p>Создайте сеть, пригласите участников через Steam<br class="wide-only" /> и подключайтесь по виртуальным IP.</p><button class="button primary" id="create-first">Создать первую сеть <span>＋</span></button><div class="readiness"><span class="readiness-dot ${online ? 'ready' : ''}"></span><div><strong>${online ? 'Steam готов к работе' : 'Сначала запустите Steam'}</strong><p>${online ? 'Конфигурация будет сохранена на этом компьютере.' : 'Приложение подключится, когда вы войдёте в аккаунт.'}</p></div></div></section>`);
    document.querySelector<HTMLButtonElement>('#create-first')!.onclick = openCreate;
    return;
  }
  const owner = active.owner === state.steam_id;
  const local = active.members.find(m => m.steam_id === state.steam_id);
  contentHTML(`<section class="network-detail" id="detail-${active.id}"><div class="detail-header"><div><h1>${escape(active.name)}</h1><p>${active.members.filter(m => m.active).length} участников</p></div><button id="invite" class="button primary" ${!owner || !online ? 'disabled' : ''}>Пригласить <span>＋</span></button></div>
    <div class="connection-banner"><span class="adapter-symbol ${active.adapter ? 'enabled' : ''}">↔</span><div><strong>${active.adapter ? 'Сеть включена' : 'Сеть выключена'}</strong><p>${local?.active ? `Ваш адрес: <code>${escape(local.ip)}</code>` : 'Ваш доступ к сети отозван'}</p></div><button class="button" id="toggle-adapter" ${!online || !local?.active ? 'disabled' : ''}>${active.adapter ? 'Выключить' : 'Включить'}</button></div>
    <div class="section-heading"><h2>Участники</h2></div><div class="table-scroll"><table><thead><tr><th>Участник</th><th>Виртуальный IP</th><th>Соединение</th><th><span class="sr-only">Действия</span></th></tr></thead><tbody>${active.members.map(peer => `<tr id="peer-${active.id}-${peer.steam_id}" class="${peer.active ? '' : 'revoked'}"><td><div class="person"><span class="initials" aria-hidden="true">${escape((peer.name || '?').slice(0, 2).toUpperCase())}</span><span><strong>${escape(peer.name || peer.steam_id)}</strong><small>${peer.steam_id === active.owner ? 'Владелец' : 'Участник'}${peer.steam_id === state.steam_id ? ' · Вы' : ''}</small></span></div></td><td><code>${escape(peer.ip)}</code></td><td>${status(peer.state)}${peer.ping_ms != null ? `<small>${peer.ping_ms} мс</small>` : ''}</td><td>${owner && peer.steam_id !== active.owner ? `<button class="text-button ${peer.active ? 'danger' : ''}" data-member="${peer.steam_id}" data-active="${peer.active}">${peer.active ? 'Удалить' : 'Вернуть'}</button>` : ''}</td></tr>`).join('')}</tbody></table></div>
    <div class="network-actions"><button class="text-button danger" id="delete-network" ${!online ? 'disabled' : ''}>Удалить сеть с компьютера</button></div></section>`);
  document.querySelector<HTMLButtonElement>('#invite')!.onclick = openInvite;
  document.querySelector<HTMLButtonElement>('#toggle-adapter')!.onclick = () => { void send({ type: 'set_adapter', network: active.id, enabled: !active.adapter }); };
  document.querySelector<HTMLButtonElement>('#delete-network')!.onclick = () => {
    const dialog = document.querySelector<HTMLDialogElement>('#delete-dialog')!;
    dialog.dataset.network = active.id;
    document.querySelector('#delete-description')!.textContent = `«${active.name}» будет удалена из сохранённых сетей.${owner ? ' Вы — владелец: для восстановления управления потребуется резервная копия конфигурации сети.' : ''}`;
    dialog.showModal();
  };
  document.querySelectorAll<HTMLButtonElement>('[data-member]').forEach(button => button.onclick = () => send({ type: button.dataset.active === 'true' ? 'revoke' : 'readmit', network: active.id, steam_id: button.dataset.member }));
}

function renderDiagnostics() {
  contentHTML(`<section class="network-detail" id="diagnostic-page"><button id="back-settings" class="text-button">← Настройки</button><div class="detail-header"><div><h1>Диагностика</h1><p>Состояние клиента и последние события</p></div></div><dl class="diagnostic-data"><dt>SteamID</dt><dd><code>${escape(state.steam_id || 'Не получен')}</code></dd><dt>Steam Datagram Relay</dt><dd>${escape(state.relay)}</dd><dt>Режим транспорта</dt><dd>Steam P2P · ICE отключён</dd><dt>Виртуальный адаптер</dt><dd>Wintun · IPv4 · MTU 1280</dd><dt>Среда</dt><dd>${isTauri() ? 'Desktop-клиент' : 'Браузерный просмотр — Steam недоступен'}</dd></dl><p id="counters"></p>${state.networks.map(n => `<details class="network-info" id="info-${n.id}"><summary>${escape(n.name)} — информация о сети</summary><dl><dt>Network ID</dt><dd><code>${n.id}</code></dd><dt>Подсеть</dt><dd>${escape(n.subnet)}</dd><dt>Версия конфигурации</dt><dd>${n.revision}</dd><dt>Steam Lobby ID</dt><dd><code>${n.lobby || 'Lobby сейчас не создано'}</code></dd></dl></details>`).join('')}<div class="section-heading"><h2>Журнал событий</h2></div><div class="event-log" id="event-log"></div><button id="join" class="button">Войти по Steam Lobby ID</button></section>`);
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
  contentHTML(`<section class="network-detail" id="settings-page"><div class="detail-header"><div><h1>Настройки</h1><p>${escape(appLabel())}</p></div></div><h2>Поведение приложения</h2><label class="setting-row" for="minimize-tray"><span>Сворачивать в трей<small>При нажатии кнопки сворачивания окна</small></span><input id="minimize-tray" type="checkbox" ${preferences.minimize_to_tray ? 'checked' : ''} /></label><p class="helper">Кнопка закрытия всегда оставляет сети работать в фоне. Для завершения работы выберите «Выйти» здесь или в меню трея.</p><div class="section-heading"><h2>Обновления</h2></div><label class="setting-row" for="auto-update"><span>Проверять при запуске<small>Установка начинается только по вашему нажатию</small></span><input id="auto-update" type="checkbox" ${preferences.check_updates ? 'checked' : ''} /></label><p class="helper" id="update-message" role="status">${escape(updateMessage)}</p><div class="settings-actions"><button class="button" id="check-update" ${updating ? 'disabled' : ''}>${updating ? 'Подождите…' : 'Проверить обновления'}</button>${updateVersion ? `<button class="button primary" id="install-update" ${updating ? 'disabled' : ''}>Установить ${escape(updateVersion)}</button>` : ''}</div><div class="section-heading"><h2>Дополнительно</h2></div><div class="settings-actions"><button id="diagnostics" class="button">Диагностика</button><button id="quit" class="button">Выйти из FreeC Tier</button></div><p class="helper">Тема: тёмно-красная</p></section>`);
  document.querySelector<HTMLInputElement>('#minimize-tray')!.onchange = e => { void savePreferences({ ...preferences, minimize_to_tray: (e.target as HTMLInputElement).checked }); };
  document.querySelector<HTMLInputElement>('#auto-update')!.onchange = e => { void savePreferences({ ...preferences, check_updates: (e.target as HTMLInputElement).checked }); };
  document.querySelector<HTMLButtonElement>('#diagnostics')!.onclick = () => { page = 'diagnostics'; render(); };
  document.querySelector<HTMLButtonElement>('#quit')!.onclick = () => { void desktop('quit'); };
  document.querySelector<HTMLButtonElement>('#check-update')!.onclick = () => { void checkUpdate(); };
  const install = document.querySelector<HTMLButtonElement>('#install-update');
  if (install) install.onclick = async () => {
    updating = true; updateMessage = 'Загрузка и проверка подписи. Затем приложение закроется для установки.'; renderSettings();
    try { await invoke('install_update'); } catch (error) { updateMessage = `Обновление не установлено: ${String(error)}. Проверьте обновления ещё раз.`; }
    finally { updating = false; updateVersion = null; if (page === 'settings') renderSettings(); }
  };
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
  updating = true; updateMessage = 'Проверяем GitHub Releases…'; updateVersion = null;
  if (page === 'settings') renderSettings();
  try {
    const result = await invoke<{configured: boolean; version: string | null}>('check_update');
    updateVersion = result.version;
    updateMessage = !result.configured ? 'Обновления не настроены в этой сборке: нужны GitHub-репозиторий и публичный ключ подписи.' : result.version ? `Доступна версия ${result.version}.` : 'Установлена актуальная версия.';
    if (result.version && page !== 'settings') notice(`Доступна версия ${result.version}. Откройте Настройки для установки.`);
  } catch (error) { updateMessage = `Не удалось проверить обновления: ${String(error)}. Попробуйте ещё раз.`; }
  finally { updating = false; if (page === 'settings') renderSettings(); }
}
function openCreate() { (document.querySelector('#create-dialog') as HTMLDialogElement).showModal(); }
function openInvite() {
  const active = network(); if (!active) return;
  document.querySelector('#friend-list')!.innerHTML = state.friends.length ? state.friends.map(f => `<button class="friend-choice" data-friend="${f.steam_id}"><span>${escape(f.name)}</span><span>Пригласить →</span></button>`).join('') : '<p class="helper">Список друзей пока пуст. Можно воспользоваться Steam Overlay.</p>';
  document.querySelectorAll<HTMLButtonElement>('[data-friend]').forEach(button => button.onclick = async () => { if (await send({ type: 'invite_friend', network: active.id, steam_id: button.dataset.friend })) { notice('Запрос приглашения передан Steam. Результат появится в диагностике.'); (document.querySelector('#invite-dialog') as HTMLDialogElement).close(); } });
  (document.querySelector('#invite-dialog') as HTMLDialogElement).showModal();
}
document.querySelector('#create')!.addEventListener('click', openCreate);
document.querySelector('#settings')!.addEventListener('click', () => { page = 'settings'; render(); });
document.querySelector('.brand')!.addEventListener('click', e => { e.preventDefault(); page = 'networks'; render(); });
document.querySelector('#overlay')!.addEventListener('click', () => { if (selected) send({ type: 'invite', network: selected }); });
document.querySelectorAll<HTMLButtonElement>('.close').forEach(button => button.onclick = () => button.closest('dialog')!.close());
document.querySelector<HTMLFormElement>('#create-form')!.onsubmit = async e => {
  e.preventDefault(); const input = document.querySelector<HTMLInputElement>('#network-name')!;
  if (await send({ type: 'create', name: input.value.trim() })) { (document.querySelector('#create-dialog') as HTMLDialogElement).close(); input.value = ''; }
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
      const serialized = JSON.stringify({ steam: next.steam, steam_id: next.steam_id, nickname: next.nickname, relay: next.relay, networks: next.networks });
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
render();
if (isTauri()) void invoke<string>('version').then(value => { if (value) { setVersion(value); if (page === 'settings') renderSettings(); } }).catch(() => {});
if (isTauri()) void invoke<Settings>('settings').then(result => { if (result) preferences = result; if (page === 'settings') renderSettings(); if (preferences.check_updates) void checkUpdate(); }).catch(error => notice(`Не удалось загрузить настройки: ${String(error)}`));
void poll();
