// Interface strings. The catalogs are `locales/*.json` at the repository root:
// flat `"section.name": "text"` maps in the shape Crowdin's key-value JSON
// reads and writes, shared with the Rust side (tray, dialogs, engine events).
// A file dropped there is picked up by the next build — nothing here lists
// the languages. English is the source and the fallback for missing keys.

import flags from 'virtual:locale-flags';

type Catalog = Record<string, string>;
type Params = Record<string, string | number>;
export type Language = { code: string; name: string; flag?: string };

export const FALLBACK = 'en';

const files = import.meta.glob<Catalog>('../../../locales/*.json', { eager: true, import: 'default' });
const catalogs = new Map<string, Catalog>();
for (const [path, catalog] of Object.entries(files)) catalogs.set(path.slice(path.lastIndexOf('/') + 1, -'.json'.length), catalog);

// Flag SVGs (Windows has no flag glyphs — the emoji render as two letters),
// bundled per shipped language by the `locale-flags` plugin in vite.config.ts.
// A language without one shows its code instead.
function flagOf(code: string): string | undefined { return flags[code]; }
// Each language is listed under its own name, the way people look for it.
function nameOf(code: string) {
  let name = code;
  try { name = new Intl.DisplayNames([code], { type: 'language' }).of(code) ?? code; } catch { /* unknown tag */ }
  return name.charAt(0).toLocaleUpperCase(code) + name.slice(1);
}
export const LANGUAGES: Language[] = [...catalogs.keys()]
  .map(code => ({ code, name: nameOf(code), flag: flagOf(code) }))
  // One fixed collation, so the list does not reorder with the browser locale.
  .sort((a, b) => a.name.localeCompare(b.name, 'en'));

let current = FALLBACK;

/** The shipped language for a BCP 47 tag: exact (`pt-BR`), then primary (`pt`). */
export function resolve(tag?: string | null): string | undefined {
  if (!tag) return undefined;
  const wanted = tag.trim().replace('_', '-').toLowerCase();
  const codes = [...catalogs.keys()];
  return codes.find(code => code.toLowerCase() === wanted) ?? codes.find(code => code.toLowerCase() === wanted.split('-')[0]);
}
export function language() { return current; }
export function setLanguage(tag?: string | null) {
  current = resolve(tag) ?? FALLBACK;
  document.documentElement.lang = current;
  // Remembered by the webview too, so the next launch paints its very first
  // frame in the right language instead of waiting for the settings call.
  try { localStorage.setItem('language', current); } catch { /* storage disabled */ }
  return current;
}

// The authoritative first-run choice is made in Rust from the Windows region
// (`i18n::pick_language`). This mirror only covers the first frame before the
// settings arrive, and the browser preview, which has nothing better.
const RUSSIAN_LANGUAGES = ['ru', 'be', 'uk', 'ka', 'hy', 'az', 'kk', 'ky', 'tg', 'tk', 'uz'];
const RUSSIAN_REGIONS = ['RU', 'BY', 'UA', 'MD', 'GE', 'AM', 'AZ', 'KZ', 'KG', 'TJ', 'TM', 'UZ'];
export function initialLanguage() {
  let stored: string | null = null;
  try { stored = localStorage.getItem('language'); } catch { /* storage disabled */ }
  if (resolve(stored)) return resolve(stored)!;
  const tag = navigator.language || '';
  const [primary = '', ...rest] = tag.split('-');
  if (primary.toLowerCase() !== FALLBACK && resolve(tag)) return resolve(tag)!;
  if (RUSSIAN_LANGUAGES.includes(primary.toLowerCase()) || rest.some(part => RUSSIAN_REGIONS.includes(part.toUpperCase()))) return resolve('ru') ?? FALLBACK;
  return FALLBACK;
}

function template(key: string) {
  return catalogs.get(current)?.[key] ?? catalogs.get(FALLBACK)?.[key] ?? key;
}
function fill(text: string, params?: Params) {
  return params ? text.replace(/\{(\w+)\}/g, (match, name: string) => (name in params ? String(params[name]) : match)) : text;
}
function escapeHTML(value: string) {
  return value.replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]!));
}
/** Plain text, for `textContent`, notices and attribute values set from code. */
export function t(key: string, params?: Params) {
  return fill(template(key), params);
}
/**
 * HTML for templates. Catalogs come back from a translation service, so the
 * translated text itself is escaped; `params` go in as given, which lets code
 * pass markup (`<code>…</code>`) — callers escape any user data they pass.
 */
export function h(key: string, params?: Params) {
  return fill(escapeHTML(template(key)), params);
}

/** Static markup carries keys in `data-i18n*` attributes; this fills them. */
export function translateStatic(root: ParentNode = document) {
  root.querySelectorAll<HTMLElement>('[data-i18n]').forEach(element => { element.textContent = t(element.dataset.i18n!); });
  for (const attribute of ['title', 'aria-label', 'placeholder', 'alt']) {
    root.querySelectorAll<HTMLElement>(`[data-i18n-${attribute}]`).forEach(element => element.setAttribute(attribute, t(element.getAttribute(`data-i18n-${attribute}`)!)));
  }
}

function flagHTML(entry: Language) {
  return entry.flag
    ? `<img class="flag" src="${escapeHTML(entry.flag)}" alt="" width="20" height="15" />`
    : `<span class="flag flag-code" aria-hidden="true">${escapeHTML(entry.code.slice(0, 2).toUpperCase())}</span>`;
}
/** A language menu: the current flag and name, opening a list of the others. */
export function languagePicker(id: string) {
  const active = LANGUAGES.find(entry => entry.code === current) ?? { code: current, name: current };
  const options = LANGUAGES.map(entry => {
    const selected = entry.code === current;
    return `<button type="button" role="option" class="lang-option ${selected ? 'active' : ''}" aria-selected="${selected}" data-language="${escapeHTML(entry.code)}" lang="${escapeHTML(entry.code)}">${flagHTML(entry)}<span class="lang-name">${escapeHTML(entry.name)}</span>${selected ? '<span class="lang-check" aria-hidden="true">✓</span>' : ''}</button>`;
  }).join('');
  return `<div class="lang-picker" id="${id}"><button type="button" class="lang-current" aria-haspopup="listbox" aria-expanded="false" aria-label="${h('language.label')}: ${escapeHTML(active.name)}" title="${h('language.label')}">${flagHTML(active)}<span class="lang-name">${escapeHTML(active.name)}</span><svg class="lang-chevron" width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M2 3.5 5 6.5 8 3.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" /></svg></button><div class="lang-menu" role="listbox" aria-label="${h('language.label')}" hidden>${options}</div></div>`;
}
function closeMenu(picker: Element) {
  const menu = picker.querySelector<HTMLElement>('.lang-menu');
  if (!menu || menu.hidden) return;
  menu.hidden = true;
  picker.querySelector('.lang-current')?.setAttribute('aria-expanded', 'false');
}
/** Wires a picker rendered by `languagePicker`; safe to call after every render. */
export function bindLanguagePicker(picker: HTMLElement | null, choose: (code: string) => void) {
  if (!picker) return;
  const button = picker.querySelector<HTMLButtonElement>('.lang-current')!;
  const menu = picker.querySelector<HTMLElement>('.lang-menu')!;
  const options = () => [...menu.querySelectorAll<HTMLButtonElement>('[data-language]')];
  button.onclick = () => {
    const opening = menu.hidden;
    document.querySelectorAll('.lang-picker').forEach(closeMenu);
    if (!opening) return;
    menu.hidden = false;
    button.setAttribute('aria-expanded', 'true');
    (options().find(option => option.classList.contains('active')) ?? options()[0])?.focus();
  };
  options().forEach(option => option.onclick = () => {
    closeMenu(picker);
    button.focus();
    if (option.dataset.language !== current) choose(option.dataset.language!);
  });
  picker.onkeydown = event => {
    if (menu.hidden) return;
    const list = options();
    const index = list.indexOf(document.activeElement as HTMLButtonElement);
    if (event.key === 'Escape') {
      // Inside a dialog Escape would close the whole dialog; the menu first.
      event.preventDefault(); event.stopPropagation();
      closeMenu(picker); button.focus();
    } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      list[(index + (event.key === 'ArrowDown' ? 1 : list.length - 1)) % list.length]?.focus();
    }
  };
}
// One document listener closes whichever menu is open when the click lands
// elsewhere — pickers re-render, so per-element listeners would pile up.
document.addEventListener('click', event => {
  document.querySelectorAll('.lang-picker').forEach(picker => { if (!picker.contains(event.target as Node)) closeMenu(picker); });
});
