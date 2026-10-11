import { readdirSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { defineConfig, searchForWorkspaceRoot, type Plugin } from 'vite';

// The interface strings live in `locales/` at the repository root, shared with
// the Rust side and synced with Crowdin — outside this package.
const locales = fileURLToPath(new URL('../../locales', import.meta.url));
const flagDir = fileURLToPath(new URL('./node_modules/flag-icons/flags/4x3', import.meta.url));

// A language is not a country; this is the flag people expect next to it.
// Unlisted languages fall back to their own region subtag (`pt-BR` → br),
// then to a text badge in the UI.
const FLAG_COUNTRY: Record<string, string> = {
  en: 'gb', ru: 'ru', uk: 'ua', be: 'by', kk: 'kz', zh: 'cn', 'zh-tw': 'tw', ja: 'jp', ko: 'kr', cs: 'cz',
  sv: 'se', da: 'dk', nb: 'no', nn: 'no', sr: 'rs', sl: 'si', el: 'gr', he: 'il', ar: 'sa', fa: 'ir',
  hi: 'in', vi: 'vn', ms: 'my', tl: 'ph', et: 'ee', ka: 'ge', hy: 'am', ga: 'ie', cy: 'gb-wls',
};
function flagCountry(code: string) {
  const lower = code.toLowerCase();
  const [primary, region] = lower.split('-');
  const candidates = [FLAG_COUNTRY[lower], region, FLAG_COUNTRY[primary], primary];
  return candidates.find(country => country && existsSync(`${flagDir}/${country}.svg`));
}

/**
 * `virtual:locale-flags` maps every language in `locales/` to its flag URL.
 * Windows ships no flag glyphs, so flags are flag-icons SVGs — and only the
 * flags of languages that actually ship are bundled, not the 2 MB full set.
 */
function localeFlags(): Plugin {
  const id = 'virtual:locale-flags';
  return {
    name: 'locale-flags',
    resolveId: source => (source === id ? `\0${id}` : undefined),
    load(source) {
      if (source !== `\0${id}`) return undefined;
      const imports: string[] = [];
      const entries: string[] = [];
      readdirSync(locales).filter(file => file.endsWith('.json')).forEach((file, index) => {
        const code = file.slice(0, -'.json'.length);
        const country = flagCountry(code);
        if (!country) return;
        imports.push(`import flag${index} from 'flag-icons/flags/4x3/${country}.svg?url';`);
        entries.push(`${JSON.stringify(code)}: flag${index}`);
      });
      return `${imports.join('\n')}\nexport default { ${entries.join(', ')} };\n`;
    },
  };
}

export default defineConfig({
  plugins: [localeFlags()],
  server: {
    port: 1420,
    strictPort: true,
    fs: { allow: [searchForWorkspaceRoot(process.cwd()), locales] },
  },
});
