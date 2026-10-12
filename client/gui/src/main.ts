import { invoke, isTauri } from '@tauri-apps/api/core';
import morphdom from 'morphdom';
import './fonts';
import './styles.css';
import { bindLanguagePicker, h, initialLanguage, language, languagePicker, setLanguage, t, translateStatic } from './i18n';

type Peer = { steam_id: string; name: string; ip: string; active: boolean; state: string; ping_ms?: number; can_invite: boolean; may_invite: boolean; can_kick: boolean; may_kick: boolean };
type Network = { id: string; name: string; subnet: string; owner: string; revision: number; adapter: boolean; lobby?: string; public: boolean; can_invite: boolean; password: boolean; members: Peer[] };
type Snapshot = { steam: string; steam_id?: string; nickname?: string; relay: string; networks: Network[]; friends: { steam_id: string; name: string }[]; events: string[]; errors?: number; last_error?: string; received: number; sent: number; dropped: number; joins?: { id: string; name: string; password: boolean }[]; public_networks?: { lobby: string; name: string }[] };
type Command = { type: string; [key: string]: unknown };
type Settings = { minimize_to_tray: boolean; check_updates: boolean; autostart?: boolean; start_in_tray?: boolean; theme: string; onboarded?: boolean; accent?: string; custom_colors?: Record<string, string>; mtu?: number; language?: string };
type SteamAppState = { app_id: number; in_library: boolean | null };

let state: Snapshot = { steam: 'waiting', relay: 'unknown', networks: [], friends: [], events: [], received: 0, sent: 0, dropped: 0 };
let selected: string | undefined;
let previous = '';
let page: 'networks' | 'settings' | 'diagnostics' | 'discover' = 'networks';
let preferences: Settings = { minimize_to_tray: true, check_updates: true, autostart: false, theme: 'dark' };
// Kept as a catalog key, not as text, so a language switch re-renders it.
let updateStatus: { key: string; params?: Record<string, string> } = { key: 'update.never' };
let updateVersion: string | null = null;
let updating = false;
let busy = false;
let appVersion = '';
let createPublic = false;
let discoverQuery = '';
let copied = 0;
let steamApp: SteamAppState | null = null;
let welcomePoll = 0;
// The «Своё» segment stays selected while the typed value happens to equal a
// preset, so entering 1280 by hand does not snap the UI back to the preset.
let mtuCustom = false;
// Mirrors freec_core::packet — the engine clamps anyway, the UI only has to
// keep the user inside the range while they type.
const MTU_MIN = 576;
const MTU_MAX = 1500;
const MTU_DEFAULT = 1280;
const MTU_PRESETS: { value: number; hint: string }[] = [
  { value: 1280, hint: 'mtu.hint_1280' },
  { value: 1152, hint: 'mtu.hint_1152' },
  { value: 1500, hint: 'mtu.hint_1500' },
];
const app = document.querySelector<HTMLDivElement>('#app')!;
// The window has no native frame (`decorations: false`): the top bar is its
// caption and these are its buttons. Glyphs follow the 10px Windows set.
const WINDOW_CHROME = `<div class="window-chrome" id="window-chrome">
    <div class="chrome-drag" data-tauri-drag-region aria-hidden="true"></div>
    <div class="window-controls" role="group" data-i18n-aria-label="window.controls" data-tauri-drag-region>
      <button class="window-button" id="window-minimize" data-i18n-title="window.minimize" data-i18n-aria-label="window.minimize"><svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" aria-hidden="true"><path d="M0 5.5h10" /></svg></button>
      <button class="window-button" id="window-maximize" data-i18n-title="window.maximize" data-i18n-aria-label="window.maximize"><svg class="glyph-maximize" width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" aria-hidden="true"><rect x=".5" y=".5" width="9" height="9" rx="1.5" /></svg><svg class="glyph-restore" width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" aria-hidden="true"><path d="M2.5 2.5v-1a1 1 0 0 1 1-1h5a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-1" /><rect x=".5" y="2.5" width="7" height="7" rx="1" /></svg></button>
      <button class="window-button close" id="window-close" data-i18n-title="window.close" data-i18n-aria-label="window.close"><svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-linecap="round" aria-hidden="true"><path d="M.75.75l8.5 8.5M9.25.75l-8.5 8.5" /></svg></button>
    </div>
  </div>`;
setLanguage(initialLanguage());
// Static markup carries catalog keys (`data-i18n*`) instead of text, so a
// language switch refills it in place — dialogs stay open through a switch.
app.innerHTML = `
  <nav class="rail" data-i18n-aria-label="nav.sections">
    <a class="brand" href="#" data-i18n-aria-label="nav.home"><span class="brand-mark" aria-hidden="true">F</span><small class="brand-version sr-only" id="brand-version"></small></a>
    <span class="rail-sep" aria-hidden="true"></span>
    <div id="network-list" role="group" data-i18n-aria-label="nav.saved_networks"></div>
    <button class="rail-button add" id="create" data-i18n-title="nav.create" data-i18n-aria-label="nav.create">+</button>
    <button class="rail-button live" id="discover" data-i18n-title="nav.public">LIVE</button>
    <span class="rail-grow"></span>
    <button class="rail-button cog" id="settings" data-i18n-title="nav.settings" data-i18n-aria-label="nav.settings"><span aria-hidden="true">⚙</span></button>
  </nav>
  <div class="main">
    <header class="topbar" data-tauri-drag-region="deep"><div class="topbar-title" id="page-title">FreeC Tier</div><span class="topbar-sub" id="page-sub"></span><span class="topbar-grow"></span><button id="header-update" class="header-update" hidden></button><div id="session" role="status"></div></header>
    <main id="content"></main>
  </div>
  <div id="notice" role="status" hidden></div>
  <div id="joins-bar" role="status" hidden></div>
  ${isTauri() ? WINDOW_CHROME : ''}
  <dialog id="create-dialog"><form id="create-form"><div class="dialog-heading"><h2 data-i18n="create.title"></h2><button class="icon-button close" type="button" data-i18n-aria-label="common.close">×</button></div><p data-i18n="create.subnet_note"></p><label for="network-name" data-i18n="create.name"></label><input id="network-name" name="name" maxlength="64" required data-i18n-placeholder="create.name_placeholder" autocomplete="off" /><label data-i18n="access.label"></label><div class="segmented wide" role="group" data-i18n-aria-label="access.label"><button type="button" id="create-private" class="seg active" aria-pressed="true" data-i18n="access.private"></button><button type="button" id="create-public" class="seg" aria-pressed="false" data-i18n="access.public"></button></div><label id="create-password-label" hidden><span data-i18n="create.password"></span><input id="create-password" type="password" maxlength="128" autocomplete="new-password" /></label><p class="hint" data-i18n="create.hint"></p><div class="dialog-actions"><button type="button" class="button close" data-i18n="common.cancel"></button><button class="button primary" type="submit" data-i18n="create.submit"></button></div></form></dialog>
  <dialog id="join-dialog"><form id="join-form"><div class="dialog-heading"><h2 data-i18n="join.title"></h2><button class="icon-button close" type="button" data-i18n-aria-label="common.close">×</button></div><label for="lobby-id" data-i18n="join.lobby_id"></label><input id="lobby-id" name="lobby" required inputmode="numeric" pattern="[0-9]{1,20}" data-i18n-placeholder="join.lobby_placeholder" /><div class="dialog-actions"><button type="button" class="button close" data-i18n="common.cancel"></button><button class="button primary" type="submit" data-i18n="join.submit"></button></div></form></dialog>
  <dialog id="invite-dialog"><div class="dialog-heading"><h2 data-i18n="invite.title"></h2><button class="icon-button close" data-i18n-aria-label="common.close">×</button></div><div id="friend-list"></div><button id="overlay" class="button" data-i18n="invite.overlay"></button></dialog>
  <dialog id="delete-dialog"><form id="delete-form"><div class="dialog-heading"><h2 data-i18n="delete.title"></h2><button class="icon-button close" type="button" data-i18n-aria-label="common.close">×</button></div><p id="delete-description"></p><p data-i18n="delete.others_keep"></p><div class="dialog-actions"><button type="button" class="button close" data-i18n="common.cancel"></button><button class="button danger" type="submit" data-i18n="delete.submit"></button></div></form></dialog>
  <dialog id="rename-dialog"><form id="rename-form"><div class="dialog-heading"><h2 data-i18n="rename.title"></h2><button class="icon-button close" type="button" data-i18n-aria-label="common.close">×</button></div><label for="rename-name" data-i18n="create.name"></label><input id="rename-name" name="name" maxlength="64" required data-i18n-placeholder="create.name_placeholder" autocomplete="off" /><div class="dialog-actions"><button type="button" class="button close" data-i18n="common.cancel"></button><button class="button primary" type="submit" data-i18n="rename.submit"></button></div></form></dialog>
  <dialog id="password-dialog"><form id="password-form"><div class="dialog-heading"><h2 id="password-title" data-i18n="password.title"></h2><button class="icon-button close" type="button" data-i18n-aria-label="common.close">×</button></div><label for="password-input" data-i18n="password.label"></label><input id="password-input" type="password" maxlength="128" autocomplete="current-password" /><p class="hint" id="password-hint"></p><div class="dialog-actions"><button class="button primary" type="submit" data-i18n="password.continue"></button></div></form></dialog>
  <dialog id="steam-help-dialog"><div class="dialog-heading"><h2 data-i18n="steam_help.title"></h2><button class="icon-button close" type="button" data-i18n-aria-label="common.close">×</button></div><p class="steam-help-note" data-i18n="steam_help.note"></p><div class="steam-help">${hideStep(1)}</div></dialog>
  <dialog id="welcome-dialog"></dialog>`;
translateStatic();

const content = document.querySelector<HTMLElement>('#content')!;
// Keyed DOM reconciliation keeps focus, open details and scroll containers alive.
function patch(element: HTMLElement, html: string) {
  const next = element.cloneNode(false) as HTMLElement; next.innerHTML = html;
  morphdom(element, next, { childrenOnly: true, onBeforeElUpdated(from, to) {
    if (from instanceof HTMLDetailsElement) to.toggleAttribute('open', from.open);
    // An open language menu must survive the welcome dialog's 2 s re-render.
    if (from.classList.contains('lang-picker') && from.querySelector('.lang-menu:not([hidden])')) return false;
    if (from.id === 'event-log' || from.id === 'counters') return false;
    return !from.isEqualNode(to);
  }, onBeforeNodeDiscarded: node => (node as Element).id !== 'window-chrome' });
}
function contentHTML(html: string) { patch(content, html); }
function escape(value: string) { return value.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]!)); }
const PEER_STATES = ['connected', 'connecting', 'reconnecting', 'offline', 'local', 'removed'];
function peerLabel(value: string) { return PEER_STATES.includes(value) ? t(`peer.${value}`) : value; }
function network() { return state.networks.find(n => n.id === selected); }
function appLabel() { return appVersion ? `FreeC Tier ${appVersion}` : 'FreeC Tier'; }
// Identity colors: avatars differ by name so the rail stays scannable, while
// saturation and lightness stay with the theme. The hue arrives as a class,
// not a style attribute — the webview CSP is `style-src 'self'` and drops
// inline styles, which silently left every avatar unpainted.
function hueClass(value: string) { let total = 0; for (const point of value || '?') total = (total * 31 + point.codePointAt(0)!) % 4093; return `hue-${total % 12}`; }
function initials(value: string) { return escape([...(value || '?')].slice(0, 2).join('').toUpperCase()); }
// Steam avatars are fetched once per member and kept for the session.
const avatars = new Map<string, string>();
function face(steamId: string | undefined, name: string) {
  const url = steamId && avatars.get(steamId);
  return url ? `<img src="${escape(url)}" alt="" />` : initials(name);
}
function quality(ping?: number) { return ping == null ? 'none' : ping < 40 ? 'good' : ping < 80 ? 'ok' : 'bad'; }
function online(peer: Peer) { return peer.active && peer.state !== 'offline' && peer.state !== 'removed'; }

function applyTheme(theme: string) {
  const root = document.documentElement;
  for (const token of THEME_TOKENS) root.style.removeProperty(`--${token}`);
  // No `custom` block in CSS: the :root defaults act as the base and the
  // user's palette is layered on top as inline variables.
  root.dataset.theme = theme === 'custom' ? 'custom' : theme === 'light' ? 'light' : 'dark';
  if (theme === 'custom') {
    for (const [token, value] of Object.entries(preferences.custom_colors ?? {})) {
      if (value) root.style.setProperty(`--${token}`, value);
    }
  } else if (preferences.accent) {
    applyAccent(preferences.accent);
  }
  paintWindowBorder();
}
// DWM draws the hairline around the frameless window; it takes the same color
// as every other hairline in the app. A canvas resolves whatever CSS color the
// token holds into the RGB the Win32 call needs.
let colorProbe: CanvasRenderingContext2D | null = null;
function paintWindowBorder() {
  if (!isTauri()) return;
  colorProbe ??= document.createElement('canvas').getContext('2d', { willReadFrequently: true });
  if (!colorProbe) return;
  colorProbe.clearRect(0, 0, 1, 1);
  colorProbe.fillStyle = getComputedStyle(document.documentElement).getPropertyValue('--line').trim() || '#000';
  colorProbe.fillRect(0, 0, 1, 1);
  const [r, g, b] = colorProbe.getImageData(0, 0, 1, 1).data;
  void invoke('window_border', { color: [r, g, b] }).catch(() => {});
}
function bindWindowChrome() {
  const chrome = document.querySelector<HTMLElement>('#window-chrome');
  if (!chrome) return;
  const root = document.documentElement;
  root.classList.add('framed');
  const control = (id: string, action: string) => {
    document.querySelector<HTMLButtonElement>(id)!.onclick = () => { void invoke('window_control', { action }).catch(error => notice(String(error))); };
  };
  control('#window-minimize', 'minimize');
  control('#window-maximize', 'maximize');
  control('#window-close', 'close');
  // Maximizing always resizes the viewport, so the glyph follows the resize
  // event — including Win+↑, Aero Snap and a double click on the caption.
  const maximize = document.querySelector<HTMLButtonElement>('#window-maximize')!;
  const syncMaximized = async () => {
    let maximized = false;
    try { maximized = (await invoke<boolean>('window_maximized')) === true; } catch { return; }
    root.classList.toggle('maximized', maximized);
    const key = maximized ? 'window.restore' : 'window.maximize';
    maximize.dataset.i18nTitle = key; maximize.dataset.i18nAriaLabel = key;
    maximize.title = t(key); maximize.setAttribute('aria-label', t(key));
  };
  window.addEventListener('resize', () => void syncMaximized());
  void syncMaximized();
  // Like the system caption, the buttons dim while another window is active.
  window.addEventListener('blur', () => root.classList.add('inactive'));
  window.addEventListener('focus', () => root.classList.remove('inactive'));
  // A modal dialog makes everything outside it inert — the caption included,
  // so the window could be neither moved nor closed while one is open. The
  // controls ride inside whichever dialog is modal and come back after.
  const seat = () => {
    const modal = [...document.querySelectorAll('dialog')].reverse().find(dialog => dialog.matches(':modal'));
    const host = modal ?? app;
    if (chrome.parentElement !== host) host.append(chrome);
    chrome.classList.toggle('over-modal', !!modal);
  };
  new MutationObserver(seat).observe(app, { subtree: true, attributeFilter: ['open'] });
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
// Three alternatives next to whatever the current theme calls its own accent.
const ACCENT_SLOTS = ['--accent-preset', '--accent-slot-1', '--accent-slot-2', '--accent-slot-3'] as const;
const ACCENT_LABELS = ['accent.default', 'accent.violet', 'accent.teal', 'accent.green'];
const THEME_TOKENS = ['accent', 'accent-hover', 'accent-soft', 'focus', 'text', 'muted', 'faint', 'bg', 'surface', 'surface-2', 'surface-3', 'line', 'line-soft', 'success', 'warning', 'danger'] as const;
// Catalog key of a theme token's label: `surface-2` → `theme.token_surface_2`.
function tokenLabel(token: string) { return t(`theme.token_${token.replace(/-/g, '_')}`); }
const THEME_GROUPS: Array<[string, string[]]> = [
  ['theme.group_base', ['bg', 'surface', 'surface-2', 'surface-3', 'text', 'muted', 'faint', 'line', 'line-soft']],
  ['theme.group_accent', ['accent', 'accent-hover', 'accent-soft', 'focus']],
  ['theme.group_status', ['success', 'warning', 'danger']],
];
// Everything on screen follows at once: static markup through its keys, the
// current page and any open dialog by re-rendering. Lines already in the
// engine's event log keep the language they were written in.
function applyLanguage(code: string) {
  setLanguage(code);
  translateStatic();
  render();
  if (document.querySelector<HTMLDialogElement>('#welcome-dialog')!.open) renderWelcome();
  if (document.querySelector<HTMLDialogElement>('#invite-dialog')!.open) openInvite();
}
function chooseLanguage(code: string) {
  applyLanguage(code);
  void savePreferences({ ...preferences, language: language() });
}
function setVersion(value: string) {
  appVersion = value;
  const brand = document.querySelector('#brand-version'); if (brand) brand.textContent = value;
  document.querySelector('.brand')!.setAttribute('title', appLabel());
}
function notice(message: string) { const el = document.querySelector<HTMLElement>('#notice')!; el.textContent = message; el.hidden = false; setTimeout(() => { el.hidden = true; }, 7000); }
async function send(command: Command): Promise<boolean> {
  if (!isTauri()) { notice(t('notice.browser_preview')); return false; }
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
  patch(document.querySelector<HTMLElement>('#session')!, `<span class="steam-pill ${connected ? 'online' : 'offline'}"><i aria-hidden="true"></i>${connected ? h('session.steam_on') : h('session.steam_off')}</span>${state.nickname ? `<span class="account"><span class="account-face ${hueClass(state.nickname)}" aria-hidden="true">${face(state.steam_id, state.nickname)}</span><span class="account-name">${escape(state.nickname)}</span></span>` : ''}`);
  patch(document.querySelector<HTMLElement>('#network-list')!, state.networks.map(n => {
    const current = n.id === selected && page === 'networks';
    return `<button id="network-${n.id}" class="rail-net ${hueClass(n.name)} ${current ? 'selected' : ''}" data-select="${n.id}" title="${escape(n.name)}" aria-label="${escape(n.name)}" ${current ? 'aria-current="page"' : ''}><span class="rail-pill" aria-hidden="true"></span><span class="rail-avatar" aria-hidden="true">${initials(n.name)}</span><span class="rail-dot ${n.adapter ? 'on' : ''}" aria-hidden="true"></span></button>`;
  }).join(''));
  document.querySelectorAll<HTMLButtonElement>('[data-select]').forEach(button => button.onclick = () => { selected = button.dataset.select; page = 'networks'; render(); });
  document.querySelector('#discover')!.classList.toggle('selected', page === 'discover');
  document.querySelector('#settings')!.classList.toggle('selected', page === 'settings' || page === 'diagnostics');
  if (page === 'diagnostics') { renderDiagnostics(); return; }
  if (page === 'settings') { renderSettings(); return; }
  if (page === 'discover') { renderDiscovery(); return; }
  const active = network();
  if (!active) {
    heading(t('home.title'), t('home.empty_sub'));
    contentHTML(`<section class="empty-state" id="empty"><svg class="empty-art" viewBox="0 0 180 110" role="img" aria-hidden="true"><path d="M42 48 L90 60 M138 48 L90 60" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" /><rect x="20" y="14" width="44" height="34" rx="9" fill="none" stroke="currentColor" stroke-width="2" /><rect x="116" y="14" width="44" height="34" rx="9" fill="none" stroke="currentColor" stroke-width="2" /><rect x="66" y="60" width="48" height="38" rx="10" fill="var(--accent)" /></svg><h1>${h('empty.title')}</h1><p>${h('empty.text')}</p><button class="button primary" id="create-first">${h('empty.create')}</button></section>`);
    document.querySelector<HTMLButtonElement>('#create-first')!.onclick = openCreate;
    return;
  }
  heading(active.name, t('network.sub', { count: active.members.length }));
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
  const statusText = h(!local?.active ? 'network.status_revoked' : on ? 'network.status_on' : 'network.status_off');
  const power = h(on ? 'network.turn_off' : 'network.turn_on');
  contentHTML(`<section class="page net-page" id="detail-${active.id}">
    <section class="hero ${on ? 'on' : ''}">
      <div class="hero-power">${on ? '<span class="hero-ring" aria-hidden="true"></span>' : ''}<button id="toggle-adapter" class="power" aria-label="${power}" title="${power}" ${!connected || !local?.active ? 'disabled' : ''}><svg width="44" height="44" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" aria-hidden="true"><path d="M12 3v8M6.3 6.3a8 8 0 1 0 11.4 0" /></svg></button></div>
      <div class="hero-body">
        <div class="hero-name"><h1>${escape(active.name)}</h1>${owner ? `<button id="rename-network" class="icon-button" title="${h('rename.aria')}" aria-label="${h('rename.aria')}"><svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 20h9" /><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" /></svg></button>` : ''}<span class="tag">${h(active.public ? 'access.public' : 'access.private')}</span></div>
        <p class="hero-status">${statusText}</p>
        <div class="hero-actions">${local?.active ? `<button id="copy-ip" class="ip-chip" title="${h('network.copy_ip')}"><span class="ip-label">${h('network.your_ip')}</span><code>${escape(local.ip)}</code><span class="ip-copy">${h(copied === 1 ? 'network.copied' : 'network.copy')}</span></button>` : ''}<button id="invite" class="button primary" ${!active.can_invite || !connected ? 'disabled' : ''}>${h('network.invite')}</button></div>
      </div>
      <div class="hero-stats"><div><strong>${present.length}<small>/${active.members.length}</small></strong><span>${h('network.online')}</span></div><div><strong>${average}<small> ${h('common.ms')}</small></strong><span>${h('network.average_ping')}</span></div></div>
    </section>
    ${!connected ? `<p class="hint warning">${h('network.steam_off_warning')}</p>` : ''}
    <div class="net-grid">
      <section class="members" aria-label="${h('network.members')}">
        <h2 class="members-head">${h('network.online_head', { count: present.length })}</h2>
        ${present.map(peer => member(active, peer, owner, connected)).join('') || `<p class="hint">${h('network.nobody')}</p>`}
        ${away.length ? `<h2 class="members-head">${h('network.offline_head', { count: away.length })}</h2>${away.map(peer => member(active, peer, owner, connected)).join('')}` : ''}
      </section>
      <aside class="net-side">
        <div class="panel"><div class="panel-head">${h('network.access_head')}</div>
          ${owner
            ? `<div class="segmented wide" role="group" aria-label="${h('access.label')}"><button id="access-private" class="seg ${active.public ? '' : 'active'}" aria-pressed="${!active.public}" ${connected ? '' : 'disabled'}>${h('access.private')}</button><button id="access-public" class="seg ${active.public ? 'active' : ''}" aria-pressed="${active.public}" ${connected ? '' : 'disabled'}>${h('access.public')}</button></div>`
            : `<p class="row-label">${h(active.public ? 'access.public' : 'access.private')}</p>`}
          <p class="hint">${h(active.public ? 'access.public_hint' : 'access.private_hint')}</p>
          ${active.public && owner ? `<button class="button wide" id="set-password" ${connected ? '' : 'disabled'}>${h(active.password ? 'network.password_set' : 'network.password_unset')}</button>` : ''}
        </div>
        <div class="panel"><div class="panel-head">${h('network.traffic_head')}</div><div class="meter"><span>↑ ${h('network.sent')}</span><b id="traffic-sent">${state.sent}</b></div><div class="meter"><span>↓ ${h('network.received')}</span><b id="traffic-received">${state.received}</b></div></div>
        <button class="side-link" id="go-diagnostics">${h('diagnostics.title')} <span aria-hidden="true">→</span></button>
        <button class="text-button danger" id="delete-network">${h('network.delete')}</button>
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
    document.querySelector('#delete-description')!.textContent = t(owner ? 'delete.owner' : 'delete.member', { name: active.name });
    dialog.showModal();
  };
  const renameButton = document.querySelector<HTMLButtonElement>('#rename-network');
  if (renameButton) renameButton.onclick = () => {
    const dialog = document.querySelector<HTMLDialogElement>('#rename-dialog')!;
    dialog.dataset.network = active.id;
    document.querySelector<HTMLInputElement>('#rename-name')!.value = active.name;
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
  if (peer.steam_id === active.owner) role.push(h('member.owner'));
  else if (peer.may_invite) role.push(h('member.can_invite'));
  if (peer.steam_id === state.steam_id) role.push(h('member.you'));
  const lit = peer.ping_ms == null ? 4 : peer.ping_ms < 40 ? 4 : peer.ping_ms < 80 ? 3 : 2;
  const manage = owner && peer.steam_id !== active.owner;
  const removable = manage || (peer.may_kick && peer.active);
  return `<div class="member ${hueClass(name)} ${here ? '' : 'off'}" id="peer-${active.id}-${peer.steam_id}" data-q="${here ? quality(peer.ping_ms) : 'none'}">
    <div class="member-face"><span class="avatar" aria-hidden="true">${face(peer.steam_id, name)}</span><span class="member-dot ${here ? escape(peer.state) : ''}" aria-hidden="true"></span></div>
    <div class="member-id"><div class="member-name"><strong>${escape(name)}</strong>${role.length ? `<span class="role ${peer.steam_id === active.owner ? 'owner' : ''}">${role.join(' · ')}</span>` : ''}</div><code>${escape(peer.ip)}</code></div>
    ${manage ? `<div class="member-perms"><label class="chip"><input type="checkbox" data-permission="invite" data-peer="${peer.steam_id}" aria-label="${h('member.invite_aria', { name: escape(name) })}" ${peer.can_invite ? 'checked' : ''} ${connected ? '' : 'disabled'} />${h('member.invite')}</label><label class="chip"><input type="checkbox" data-permission="kick" data-peer="${peer.steam_id}" aria-label="${h('member.kick_aria', { name: escape(name) })}" ${peer.can_kick ? 'checked' : ''} ${connected ? '' : 'disabled'} />${h('member.kick')}</label></div>` : ''}
    ${here
      ? `<div class="bars" role="img" aria-label="${escape(peerLabel(peer.state))}">${[0, 1, 2, 3].map(i => `<i class="${i < lit ? 'on' : ''}"></i>`).join('')}</div><div class="member-ping">${peer.ping_ms == null ? '—' : `${peer.ping_ms} ${h('common.ms')}`}</div>`
      : `<span class="member-state">${escape(peer.active ? peerLabel(peer.state) : t('peer.removed'))}</span>`}
    ${removable ? `<button class="text-button danger" data-member="${peer.steam_id}" data-active="${peer.active}" ${connected ? '' : 'disabled'}>${h(peer.active ? 'member.revoke' : 'member.readmit')}</button>` : ''}
  </div>`;
}
async function copyAddress(ip: string) {
  try { await navigator.clipboard.writeText(ip); } catch { notice(t('notice.copy_failed', { ip })); return; }
  copied = 1; render();
  setTimeout(() => { copied = 0; render(); }, 1600);
}

// Hiding the game is both the second first-run step and its own entry in
// Settings, so the instruction exists once. The keys ride along as `data-i18n`
// so the copy in the static Steam-help dialog follows a language switch too.
function hideStep(number: number) {
  return `<section class="step"><span class="step-num">${number}</span><div class="step-body"><h3 data-i18n="hide.title">${h('hide.title')}</h3><p data-i18n="hide.text">${h('hide.text')}</p><img class="step-shot" src="/steam/library.png" alt="${h('hide.image_alt')}" data-i18n-alt="hide.image_alt" loading="lazy" /></div></section>`;
}
// The library step is a prerequisite, not advice: without a license for the
// AppID the client connects under, Steam refuses the connection outright and
// every other diagnostic still looks clean. So the app opens on it.
function welcomeHTML() {
  const id = steamApp?.app_id;
  const known = id === 324810 ? h('welcome.known_game', { game: '<b>TOXIKK</b>' }) : '';
  const done = steamApp?.in_library === true;
  const status = done
    ? `<p class="step-state ok">${h('welcome.in_library')}</p>`
    : steamApp?.in_library === false
      ? `<p class="step-state warn">${h('welcome.not_in_library')}</p>`
      : `<p class="step-state">${h('welcome.checking')}</p>`;
  // Hidden once the license is there: the same deep link would launch the
  // game for somebody who also installed it.
  const add = done ? '' : `<button class="button primary" id="welcome-add">${h('welcome.add')}</button>`;
  const note = done ? '' : `<p class="hint">${h('welcome.add_note')}</p>`;
  // The language menu sits in the heading: the first thing a newcomer reads is
  // this dialog, so it is where a wrong guess about their language gets fixed.
  return `<div class="dialog-heading"><h2>${h('welcome.title')}</h2>${languagePicker('welcome-language')}</div>
    <p>${h('welcome.intro')}</p>
    <div class="steam-help">
      <section class="step"><span class="step-num">1</span><div class="step-body"><h3>${h('welcome.step1_title')}</h3><p>${h('welcome.step1_text', { app_id: `<code>${id ?? '…'}</code>`, game: known })}</p><div class="step-actions">${add}<button class="text-button" id="welcome-store">${h('welcome.store')}</button></div>${note}${status}</div></section>
      ${hideStep(2)}
    </div>
    <div class="dialog-actions"><button class="button" id="welcome-later">${h('welcome.later')}</button><button class="button primary" id="welcome-done">${h('welcome.done')}</button></div>`;
}
function renderWelcome() {
  const dialog = document.querySelector<HTMLDialogElement>('#welcome-dialog')!;
  patch(dialog, welcomeHTML());
  // Steam needs a moment to register the license, and the poll keeps looking
  // after that in case the user takes longer in the client.
  const toSteam = (store: boolean) => async () => {
    if (await desktop('open_steam_app', { store })) setTimeout(() => void refreshSteamApp(), 2000);
  };
  const add = document.querySelector<HTMLButtonElement>('#welcome-add');
  if (add) add.onclick = toSteam(false);
  document.querySelector<HTMLButtonElement>('#welcome-store')!.onclick = toSteam(true);
  document.querySelector<HTMLButtonElement>('#welcome-later')!.onclick = () => closeWelcome(false);
  document.querySelector<HTMLButtonElement>('#welcome-done')!.onclick = () => closeWelcome(true);
  bindLanguagePicker(dialog.querySelector('#welcome-language'), chooseLanguage);
  dialog.querySelectorAll<HTMLImageElement>('.step-shot').forEach(image => image.addEventListener('error', () => image.classList.add('missing')));
}
async function refreshSteamApp() {
  if (!isTauri()) return;
  try { steamApp = await invoke<SteamAppState>('steam_app'); } catch { return; }
  if (document.querySelector<HTMLDialogElement>('#welcome-dialog')!.open) renderWelcome();
}
function openWelcome() {
  renderWelcome();
  const dialog = document.querySelector<HTMLDialogElement>('#welcome-dialog')!;
  if (!dialog.open) dialog.showModal();
  void refreshSteamApp();
  // The user acts in Steam, not here, so keep looking until the license lands.
  window.clearInterval(welcomePoll);
  welcomePoll = window.setInterval(() => void refreshSteamApp(), 2000);
}
function closeWelcome(done: boolean) {
  window.clearInterval(welcomePoll);
  document.querySelector<HTMLDialogElement>('#welcome-dialog')!.close();
  if (done && !preferences.onboarded) void savePreferences({ ...preferences, onboarded: true });
}

function openPassword(id: string, editing: boolean) {
  const dialog = document.querySelector<HTMLDialogElement>('#password-dialog')!;
  dialog.dataset.network = id; dialog.dataset.editing = String(editing);
  const title = document.querySelector<HTMLElement>('#password-title')!;
  title.dataset.i18n = editing ? 'password.change_title' : 'password.enter_title';
  title.textContent = t(title.dataset.i18n);
  const hint = document.querySelector<HTMLElement>('#password-hint')!;
  hint.dataset.i18n = editing ? 'password.change_hint' : 'password.enter_hint';
  hint.textContent = t(hint.dataset.i18n);
  document.querySelector<HTMLInputElement>('#password-input')!.value = '';
  dialog.showModal();
}
// Pending admissions follow the user across pages instead of forcing a page.
function renderJoinsBar() {
  const bar = document.querySelector<HTMLDivElement>('#joins-bar')!;
  const joins = page === 'discover' ? [] : state.joins || [];
  bar.hidden = !joins.length;
  if (!joins.length) return;
  bar.innerHTML = joins.map(j => `<div class="join-chip"><span>${escape(j.name)}</span><small>${h(j.password ? 'join.password_required' : 'peer.connecting')}</small>${j.password ? `<button class="button" data-password="${j.id}">${h('password.label')}</button>` : ''}<button class="text-button" data-cancel-join="${j.id}">${h('common.cancel')}</button></div>`).join('');
  document.querySelectorAll<HTMLButtonElement>('[data-password]').forEach(b => b.onclick = () => openPassword(b.dataset.password!, false));
  document.querySelectorAll<HTMLButtonElement>('[data-cancel-join]').forEach(b => b.onclick = () => { void send({ type: 'cancel_join', network: b.dataset.cancelJoin }); });
}
// An available update is always one click away in the header, on any page.
function renderHeaderUpdate() {
  const chip = document.querySelector<HTMLButtonElement>('#header-update')!;
  if (!updateVersion) { chip.hidden = true; return; }
  chip.hidden = false;
  chip.disabled = updating;
  chip.textContent = updating ? t('update.updating') : t('update.to', { version: updateVersion });
  chip.onclick = () => { void installUpdate(); };
}
async function installUpdate() {
  updating = true; updateStatus = { key: 'update.downloading' };
  renderHeaderUpdate();
  if (page === 'settings') renderSettings();
  try { await invoke('install_update'); } catch (error) { updateStatus = { key: 'update.failed', params: { error: String(error) } }; notice(t(updateStatus.key, updateStatus.params)); }
  finally { updating = false; updateVersion = null; renderHeaderUpdate(); if (page === 'settings') renderSettings(); }
}
function renderDiscovery() {
  const joins = state.joins || [];
  const query = discoverQuery.trim().toLowerCase();
  const found = (state.public_networks || []).filter(n => !query || n.name.toLowerCase().includes(query));
  heading(t('discover.title'), t('discover.found', { count: found.length }));
  contentHTML(`<section class="page" id="discovery-page">
    <div class="page-head"><div><h1>${h('discover.title')}</h1><p class="page-sub">${h('discover.sub')}</p></div><div class="page-actions"><input id="discover-search" class="search" placeholder="${h('discover.search')}" aria-label="${h('discover.search')}" autocomplete="off" /><button class="button" id="refresh-public" ${state.steam === 'online' ? '' : 'disabled'}>${h('discover.refresh')}</button></div></div>
    ${joins.map(j => `<div class="card" id="pending-${j.id}"><div class="row"><span class="row-label">${escape(j.name)}<small>${h(j.password ? 'join.password_required' : 'peer.connecting')}</small></span><div class="row-actions">${j.password ? `<button class="button primary" data-password="${j.id}">${h('join.enter_password')}</button>` : ''}<button class="text-button" data-cancel-join="${j.id}">${h('common.cancel')}</button></div></div></div>`).join('')}
    ${found.length
      ? `<div class="discover-grid">${found.map(n => `<div class="pub-card ${hueClass(n.name)}"><div class="pub-head"><span class="avatar" aria-hidden="true">${initials(n.name)}</span><div class="pub-id"><b>${escape(n.name)}</b><small>${h('discover.public_network')}</small></div></div><button class="button primary" data-lobby="${escape(n.lobby)}">${h('discover.join')}</button></div>`).join('')}</div>`
      : `<div class="card"><p class="hint">${h(query ? 'discover.nothing_found' : 'discover.none')}</p></div>`}
    </section>`);
  document.querySelector<HTMLButtonElement>('#refresh-public')!.onclick = () => { void send({ type: 'discover' }); };
  document.querySelector<HTMLInputElement>('#discover-search')!.oninput = event => { discoverQuery = (event.target as HTMLInputElement).value; renderDiscovery(); };
  document.querySelectorAll<HTMLButtonElement>('[data-lobby]').forEach(b => b.onclick = () => { void send({ type: 'join', lobby: b.dataset.lobby }); });
  document.querySelectorAll<HTMLButtonElement>('[data-password]').forEach(b => b.onclick = () => openPassword(b.dataset.password!, false));
  document.querySelectorAll<HTMLButtonElement>('[data-cancel-join]').forEach(b => b.onclick = () => { void send({ type: 'cancel_join', network: b.dataset.cancelJoin }); });
}

function renderDiagnostics() {
  heading(t('diagnostics.title'), t('diagnostics.sub'));
  contentHTML(`<section class="page narrow" id="diagnostic-page"><button id="back-settings" class="text-button back">← ${h('settings.title')}</button><div class="page-head"><div><h1>${h('diagnostics.title')}</h1><p class="page-sub">${h('diagnostics.page_sub')}</p></div></div><div class="panel"><dl class="dl"><dt>SteamID</dt><dd><code>${state.steam_id ? escape(state.steam_id) : h('diagnostics.not_received')}</code></dd><dt>Steam Relay</dt><dd>${escape(state.relay)}</dd><dt>${h('diagnostics.transport')}</dt><dd>${h('diagnostics.transport_value')}</dd><dt>${h('diagnostics.adapter')}</dt><dd>Wintun · IPv4 · MTU ${mtuValue()}</dd><dt>${h('diagnostics.environment')}</dt><dd>${h(isTauri() ? 'diagnostics.desktop' : 'diagnostics.browser')}</dd></dl></div><p class="counters" id="counters"></p>${state.networks.map(n => `<details class="net" id="info-${n.id}"><summary>${escape(n.name)}</summary><dl class="dl"><dt>Network ID</dt><dd><code>${n.id}</code></dd><dt>${h('diagnostics.subnet')}</dt><dd>${escape(n.subnet)}</dd><dt>${h('diagnostics.revision')}</dt><dd>${n.revision}</dd><dt>Lobby ID</dt><dd><code>${n.lobby || '—'}</code></dd></dl></details>`).join('')}<div class="section-head"><h2>${h('diagnostics.events')}</h2></div><div class="event-log" id="event-log"></div><div class="page-actions"><button id="join" class="button">${h('diagnostics.join_lobby')}</button></div></section>`);
  document.querySelector<HTMLButtonElement>('#back-settings')!.onclick = () => { page = 'settings'; render(); };
  document.querySelector<HTMLButtonElement>('#join')!.onclick = () => document.querySelector<HTMLDialogElement>('#join-dialog')!.showModal();
  updateDynamic();
}
function updateDynamic() {
  const counters = document.querySelector('#counters');
  if (counters) counters.textContent = t('diagnostics.counters', { sent: state.sent, received: state.received, dropped: state.dropped });
  const sent = document.querySelector('#traffic-sent');
  if (sent) { sent.textContent = String(state.sent); document.querySelector('#traffic-received')!.textContent = String(state.received); }
  const log = document.querySelector<HTMLElement>('#event-log');
  if (log) patch(log, state.events.length ? state.events.slice().reverse().map(e => `<p>${escape(e)}</p>`).join('') : `<p>${h('diagnostics.no_events')}</p>`);
}
function renderSettings() {
  heading(t('settings.title'), appLabel());
  const theme = (name: string, key: string) => `<button id="theme-${name}" class="seg ${preferences.theme === name ? 'active' : ''}" aria-pressed="${preferences.theme === name}">${h(key)}</button>`;
  const toggle = (id: string, key: string, checked: boolean | undefined, extra = '', hint?: string) => `<label class="row"><span class="row-label">${h(key)}${hint ? `<small>${h(hint)}</small>` : ''}</span><input id="${id}" type="checkbox" class="switch" ${checked ? 'checked' : ''} ${extra} /></label>`;
  contentHTML(`<section class="page narrow" id="settings-page"><div class="page-head"><div><h1>${h('settings.title')}</h1><p class="page-sub">${h('settings.sub')}</p></div></div>
    <div class="card"><div class="row"><span class="row-label">${h('language.label')}<small>${h('language.hint')}</small></span>${languagePicker('settings-language')}</div><div class="row"><span class="row-label">${h('settings.theme')}</span><div class="segmented" role="group" aria-label="${h('settings.theme')}">${theme('dark', 'theme.dark')}${theme('light', 'theme.light')}${theme('custom', 'theme.custom')}</div></div>${preferences.theme === 'custom' ? '' : `<div class="row"><span class="row-label">${h('settings.accent')}<small>${h('settings.accent_hint')}</small></span><div class="swatches">${accentSwatches()}</div></div>`}${toggle('minimize-tray', 'settings.minimize_tray', preferences.minimize_to_tray)}${toggle('autostart', 'settings.autostart', preferences.autostart)}${toggle('start-tray', 'settings.start_tray', preferences.start_in_tray, preferences.autostart ? '' : 'disabled', 'settings.start_tray_hint')}</div>
    ${preferences.theme === 'custom' ? `<div class="card" id="theme-editor">${THEME_GROUPS.map(([title, tokens]) => `<h3 class="editor-title">${h(title)}</h3>${tokens.map(token => `<label class="row color-row"><span class="row-label">${escape(tokenLabel(token))}</span><input type="color" class="color-input" data-token="${token}" value="${escape(preferences.custom_colors?.[token] ?? '#000000')}" aria-label="${escape(tokenLabel(token))}" /></label>`).join('')}`).join('')}<div class="row"><span class="row-label muted">${h('theme.applied')}</span><button class="button" id="theme-reset">${h('theme.reset')}</button></div></div>` : ''}
    <h2 class="group-title">${h('settings.network')}</h2><div class="card"><div class="row"><span class="row-label">${h('mtu.title')}<small>${escape(mtuHint())}</small></span><div class="segmented" role="group" aria-label="MTU">${mtuSegments()}</div></div>${mtuIsCustom() ? `<label class="row"><span class="row-label">${h('mtu.custom_value')}<small>${h('mtu.range', { min: MTU_MIN, max: MTU_MAX })}</small></span><input id="mtu-input" class="number-input" type="number" inputmode="numeric" min="${MTU_MIN}" max="${MTU_MAX}" value="${mtuValue()}" aria-label="${h('mtu.custom_aria')}" /></label>` : ''}<div class="row"><span class="row-label muted">${h('mtu.applies')}</span></div></div>
    <h2 class="group-title">${h('settings.service')}</h2><div class="card"><div class="row"><span class="row-label">${h('service.name')}<small id="service-status">${h('update.checking')}</small></span><div class="row-actions"><button class="button" id="service-install">${h('service.reinstall')}</button><button class="button" id="service-uninstall">${h('service.uninstall')}</button></div></div></div>
    <h2 class="group-title">${h('settings.updates')}</h2><div class="card">${toggle('auto-update', 'settings.check_updates', preferences.check_updates, '', 'settings.check_updates_hint')}<div class="row"><span class="row-label muted" id="update-message" role="status">${h(updateStatus.key, updateStatus.params && Object.fromEntries(Object.entries(updateStatus.params).map(([k, v]) => [k, escape(v)])))}</span><div class="row-actions"><button class="button" id="check-update" ${updating ? 'disabled' : ''}>${h(updating ? 'update.checking' : 'update.check')}</button>${updateVersion ? `<button class="button primary" id="install-update" ${updating ? 'disabled' : ''}>${h('update.install', { version: escape(updateVersion) })}</button>` : ''}</div></div></div>
    <h2 class="group-title">${h('settings.steam')}</h2><div class="card"><button class="row row-button" id="steam-setup"><span class="row-label">${h('settings.steam_setup')}<small>${h('settings.steam_setup_hint')}</small></span><span class="chev" aria-hidden="true">→</span></button><button class="row row-button" id="steam-help"><span class="row-label">${h('settings.steam_hide')}</span><span class="chev" aria-hidden="true">→</span></button></div>
    <h2 class="group-title">${h('settings.more')}</h2><div class="card"><button class="row row-button" id="diagnostics"><span class="row-label">${h('diagnostics.title')}</span><span class="chev" aria-hidden="true">→</span></button><div class="row"><span class="row-label">${h('settings.quit_label')}</span><button class="button" id="quit">${h('settings.quit')}</button></div></div></section>`);
  bindLanguagePicker(document.querySelector('#settings-language'), chooseLanguage);
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
  document.querySelectorAll<HTMLButtonElement>('[data-mtu]').forEach(button => button.onclick = () => {
    const choice = button.dataset.mtu!;
    if (choice === 'custom') { mtuCustom = true; renderSettings(); return; }
    mtuCustom = false;
    void savePreferences({ ...preferences, mtu: Number(choice) });
  });
  const mtuInput = document.querySelector<HTMLInputElement>('#mtu-input');
  // On change, not on input: a half-typed «15» would otherwise be saved as 576
  // and reconfigure every live adapter on the way to 1500.
  if (mtuInput) mtuInput.onchange = () => {
    const value = Math.min(MTU_MAX, Math.max(MTU_MIN, Math.round(Number(mtuInput.value) || MTU_DEFAULT)));
    mtuInput.value = String(value);
    void savePreferences({ ...preferences, mtu: value });
  };
    async function refreshServiceStatus() {
    if (!isTauri()) return;
    try {
      const status = await invoke<{ installed: boolean; running: boolean }>('service_status');
      const label = document.querySelector('#service-status');
      if (label) label.textContent = t(status.running ? 'service.running' : status.installed ? 'service.stopped' : 'service.missing');
    } catch { /* service missing from this build */ }
  }
  void refreshServiceStatus();
  document.querySelector<HTMLButtonElement>('#service-install')!.onclick = async () => {
    if (await desktop('install_service')) setTimeout(() => void refreshServiceStatus(), 2500);
  };
  document.querySelector<HTMLButtonElement>('#service-uninstall')!.onclick = async () => {
    if (await desktop('uninstall_service')) setTimeout(() => void refreshServiceStatus(), 2500);
  };

  // The Run key is the source of truth, and it carries both answers in one
  // command line: persist the preferences only after it was rewritten, and
  // revert the switch on failure.
  const autostart = async (enabled: boolean, tray: boolean) => {
    if (!(await desktop('set_autostart', { enabled, tray }))) return false;
    void savePreferences({ ...preferences, autostart: enabled, start_in_tray: tray });
    return true;
  };
  document.querySelector<HTMLInputElement>('#autostart')!.onchange = async e => {
    const input = e.target as HTMLInputElement;
    if (!(await autostart(input.checked, preferences.start_in_tray ?? false))) input.checked = !input.checked;
  };
  document.querySelector<HTMLInputElement>('#start-tray')!.onchange = async e => {
    const input = e.target as HTMLInputElement;
    if (!(await autostart(preferences.autostart ?? false, input.checked))) input.checked = !input.checked;
  };
  document.querySelector<HTMLInputElement>('#auto-update')!.onchange = e => { void savePreferences({ ...preferences, check_updates: (e.target as HTMLInputElement).checked }); };
  document.querySelector<HTMLButtonElement>('#diagnostics')!.onclick = () => { page = 'diagnostics'; render(); };
  document.querySelector<HTMLButtonElement>('#quit')!.onclick = () => { void desktop('quit'); };
  document.querySelector<HTMLButtonElement>('#steam-setup')!.onclick = openWelcome;
  document.querySelector<HTMLButtonElement>('#steam-help')!.onclick = () => (document.querySelector('#steam-help-dialog') as HTMLDialogElement).showModal();
  document.querySelector<HTMLButtonElement>('#check-update')!.onclick = () => { void checkUpdate(); };
  const install = document.querySelector<HTMLButtonElement>('#install-update');
  if (install) install.onclick = () => { void installUpdate(); };
}
function mtuValue() {
  const stored = Number(preferences.mtu);
  return Number.isFinite(stored) && stored > 0 ? Math.min(MTU_MAX, Math.max(MTU_MIN, Math.round(stored))) : MTU_DEFAULT;
}
// A value that is not one of the presets can only have come from the field, so
// the custom row stays open across a re-render and after a restart.
function mtuIsCustom() {
  return mtuCustom || !MTU_PRESETS.some(preset => preset.value === mtuValue());
}
function mtuSegments() {
  const custom = mtuIsCustom();
  const presets = MTU_PRESETS.map(preset => {
    const active = !custom && preset.value === mtuValue();
    return `<button class="seg ${active ? 'active' : ''}" data-mtu="${preset.value}" aria-pressed="${active}">${preset.value}</button>`;
  }).join('');
  return `${presets}<button class="seg ${custom ? 'active' : ''}" data-mtu="custom" aria-pressed="${custom}">${h('mtu.custom')}</button>`;
}
function mtuHint() {
  if (mtuIsCustom()) return t('mtu.custom_hint', { value: mtuValue() });
  const preset = MTU_PRESETS.find(entry => entry.value === mtuValue());
  return preset ? t(preset.hint) : '';
}
// Swatch colors live in the stylesheet (`--accent-preset`, `--accent-slot-N`)
// and are read back here only to store the pick — a `style` attribute would be
// dropped by the CSP, and the palette belongs in CSS anyway.
function accentSwatches() {
  const style = getComputedStyle(document.documentElement);
  const colors = ACCENT_SLOTS.map(token => style.getPropertyValue(token).trim());
  const current = (preferences.accent || colors[0]).toLowerCase();
  return colors.map((color, slot) => {
    const active = color.toLowerCase() === current;
    // Slot 0 stores an empty value — «follow the theme» — so switching
    // dark ↔ light keeps each preset's own accent instead of pinning one.
    return `<button class="swatch ${active ? 'active' : ''}" data-slot="${slot}" data-accent="${slot ? escape(color) : ''}" aria-label="${h('accent.aria', { name: h(ACCENT_LABELS[slot]) })}" aria-pressed="${active}"></button>`;
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
  if (!isTauri()) { notice(t('notice.browser_settings')); return false; }
  try { await invoke(command, args); return true; } catch (error) { notice(String(error)); return false; }
}
async function savePreferences(next: Settings) {
  if (await desktop('save_settings', { settings: next })) preferences = next;
  if (page === 'settings') renderSettings();
}
// A background check is silent: it never flashes «Проверяем…», never clears an
// update already on offer, and a failed attempt (offline laptop) says nothing —
// the next one is five minutes away.
let lastUpdateCheck = 0;
async function checkUpdate(quiet = false) {
  if (!isTauri()) { if (!quiet) notice(t('notice.browser_updates')); return; }
  if (updating) return;
  lastUpdateCheck = Date.now();
  updating = true;
  if (!quiet) { updateStatus = { key: 'update.checking' }; updateVersion = null; }
  if (page === 'settings') renderSettings();
  try {
    const result = await invoke<{ configured: boolean; version: string | null }>('check_update');
    const announced = result.version !== null && result.version !== updateVersion;
    updateVersion = result.version;
    updateStatus = !result.configured ? { key: 'update.not_configured' } : result.version ? { key: 'update.available', params: { version: result.version } } : { key: 'update.latest' };
    if (announced && page !== 'settings') notice(t('update.available', { version: result.version! }));
  } catch (error) { if (!quiet) updateStatus = { key: 'update.check_failed', params: { error: String(error) } }; }
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
  document.querySelector('#friend-list')!.innerHTML = `${state.friends.length ? state.friends.map(f => `<button class="friend-choice" data-friend="${f.steam_id}"><span>${escape(f.name)}</span><span>${h('invite.action')}</span></button>`).join('') : `<p class="muted">${h('invite.no_friends')}</p>`}`;
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
document.querySelector<HTMLFormElement>('#rename-form')!.onsubmit = async e => {
  e.preventDefault(); const dialog = document.querySelector<HTMLDialogElement>('#rename-dialog')!;
  const input = document.querySelector<HTMLInputElement>('#rename-name')!;
  if (await send({ type: 'rename', network: dialog.dataset.network, name: input.value.trim() })) dialog.close();
};

// Avatars arrive out of band: ask only for the members still missing one, and
// keep asking while Steam downloads them in the background.
async function loadAvatars() {
  const missing = new Set<string>();
  for (const network of state.networks) for (const peer of network.members) if (!avatars.has(peer.steam_id)) missing.add(peer.steam_id);
  if (state.steam_id && !avatars.has(state.steam_id)) missing.add(state.steam_id);
  if (!missing.size) return;
  try {
    const found = await invoke<Record<string, string>>('avatars', { ids: [...missing] });
    const entries = Object.entries(found || {});
    if (!entries.length) return;
    for (const [id, url] of entries) avatars.set(id, url);
    render();
  } catch { /* build without the avatar command */ }
}
let tick = 0;
async function refresh() {
  if (isTauri()) {
    if (++tick % 2 === 0) void loadAvatars();
    try {
      const next = await invoke<Snapshot>('snapshot');
      const serialized = JSON.stringify({ steam: next.steam, steam_id: next.steam_id, nickname: next.nickname, relay: next.relay, networks: next.networks, joins: next.joins, public_networks: next.public_networks });
      // The engine counts its errors, so a new one is recognized whatever
      // language its line was written in.
      if ((next.errors ?? 0) > (state.errors ?? 0) && next.last_error) notice(next.last_error);
      state = next;
      if (serialized !== previous) {
        previous = serialized;
        if (!selected || !state.networks.some(n => n.id === selected)) selected = state.networks[0]?.id;
        render();
      }
      updateDynamic();
    } catch (error) { notice(t('notice.core_unreachable', { error: String(error) })); }
  }
}
// A window in the tray renders nothing, so neither the snapshot clone nor the
// avatar fetch is worth waking it for — the webview is usually suspended at
// that point anyway. Becoming visible refreshes at once instead of showing a
// second of stale state.
async function poll() {
  if (!document.hidden) await refresh();
  setTimeout(poll, 1000);
}
document.addEventListener('visibilitychange', () => { if (!document.hidden) void refresh(); });
bindWindowChrome();
applyTheme(preferences.theme);
render();
// The webview's default context menu (back/reload/print/view source) has no
// place in this utility; right click stays inert until custom actions exist.
if (isTauri()) document.addEventListener('contextmenu', e => e.preventDefault());
if (isTauri()) void invoke<string>('version').then(value => { if (value) { setVersion(value); if (page === 'settings') renderSettings(); } }).catch(() => {});
document.querySelector('#welcome-dialog')!.addEventListener('close', () => window.clearInterval(welcomePoll));
if (isTauri()) void invoke<Settings>('settings').then(result => {
  if (result) { preferences = result; applyTheme(preferences.theme); }
  // The engine settled the language on first run from the Windows region; the
  // first frame went out on a guess, so correct it if they differ.
  if (preferences.language && preferences.language !== language()) applyLanguage(preferences.language);
  if (page === 'settings') renderSettings();
  // The Steam library step gates every connection, so a client that has not
  // been through it opens on it instead of on an empty network list.
  if (!preferences.onboarded) openWelcome();
  if (preferences.check_updates) void checkUpdate();
}).catch(error => notice(t('notice.settings_failed', { error: String(error) })));
void poll();
// Periodic update check while the app runs. The setting is read at each tick,
// so the switch takes effect without a restart. A hidden window's webview is
// suspended and its timers stall, so the missed check happens the moment the
// window comes back instead.
const UPDATE_INTERVAL = 5 * 60 * 1000;
function updateDue() { return isTauri() && preferences.check_updates && !document.hidden && Date.now() - lastUpdateCheck >= UPDATE_INTERVAL; }
window.setInterval(() => { if (updateDue()) void checkUpdate(true); }, 30 * 1000);
document.addEventListener('visibilitychange', () => { if (updateDue()) void checkUpdate(true); });
