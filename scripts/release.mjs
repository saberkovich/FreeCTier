// Run build.bat --no-pause first. This builds an NSIS installer; it never publishes.
import { readFile, writeFile, readdir, copyFile, mkdir } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const gui = path.join(root, 'client/gui');
const packageVersion = JSON.parse(await readFile(path.join(gui, 'package.json'), 'utf8')).version;
const signed = process.argv.includes('--signed');
const repo = process.env.FREECTIER_GITHUB_REPOSITORY;
if (signed && (!/^[\w.-]+\/[\w.-]+$/.test(repo || '') || !process.env.FREECTIER_UPDATER_PUBLIC_KEY || !process.env.TAURI_SIGNING_PRIVATE_KEY)) {
  throw new Error('Signed release needs FREECTIER_GITHUB_REPOSITORY, FREECTIER_UPDATER_PUBLIC_KEY and TAURI_SIGNING_PRIVATE_KEY.');
}
// Tauri config expects the base64 minisign public key (the `.key.pub` content).
// Accept either that form or the decoded two-line text pasted from documentation.
function normalizePublicKey(raw) {
  const value = raw.trim();
  if (!value) throw new Error('FREECTIER_UPDATER_PUBLIC_KEY is empty.');
  if (value.includes('untrusted comment:')) return Buffer.from(value, 'utf8').toString('base64');
  return value.replace(/\s+/g, '');
}
// Release version is taken from the tag so a pushed tag defines the build.
// Precedence: FREECTIER_VERSION, then the git tag, then package.json.
const tag = process.env.GITHUB_REF_TYPE === 'tag' ? process.env.GITHUB_REF_NAME : null;
const version = (process.env.FREECTIER_VERSION || tag || packageVersion).replace(/^v/, '');
if (!/^\d+\.\d+\.\d+([-+][\w.-]+)?$/.test(version)) throw new Error(`Invalid release version "${version}". Use semver like 0.2.1 or 0.2.1-preview.`);
if (tag && tag.replace(/^v/, '') !== version) console.warn(`Warning: git tag "${tag}" does not match release version "${version}".`);

const target = path.join(root, 'target/x86_64-pc-windows-msvc/release');
const portable = path.join(root, 'dist/FreeC-Tier-release');
const output = path.join(root, 'dist/releases');
await mkdir(output, { recursive: true });
await mkdir(path.join(root, '.cache'), { recursive: true });
const buildDirs = await readdir(path.join(target, 'build'));
let icon;
for (const dir of buildDirs.filter(d => d.startsWith('freec-tier-'))) {
  try { icon = await readFile(path.join(target, 'build', dir, 'out/freec.ico')); break; } catch (e) { if (e.code !== 'ENOENT') throw e; }
}
if (!icon) throw new Error('Run build.bat --no-pause first');
const iconPath = path.join(root, '.cache/release.ico');
await writeFile(iconPath, icon);
const config = {
  version,
  bundle: {
    active: true, targets: ['nsis'], createUpdaterArtifacts: signed, icon: [iconPath],
    resources: Object.fromEntries(['steam_api64.dll', 'wintun.dll', 'WINTUN-LICENSE.txt', 'freec-service.exe'].map(name => [path.join(portable, name), name])),
    windows: {
      // The installer carries the WebView2 bootstrapper and installs the
      // runtime automatically when the machine lacks it. webviewInstallMode
      // belongs to bundle.windows, not to the nsis section.
      webviewInstallMode: { type: 'embedBootstrapper' },
      nsis: {
        installMode: 'perMachine',
        languages: ['Russian', 'English'],
        displayLanguageSelector: true,
        installerHooks: path.join(gui, 'src-tauri', 'installer-hooks.nsh'),
      },
    },
  },
};
// Without a decodable pubkey Tauri aborts updater signing, so only pass it when signing.
if (signed) config.plugins = { updater: { pubkey: normalizePublicKey(process.env.FREECTIER_UPDATER_PUBLIC_KEY) } };
const configPath = path.join(root, '.cache/release.conf.json');
await writeFile(configPath, JSON.stringify(config, null, 2));
execFileSync(process.execPath, [path.join(gui, 'node_modules/@tauri-apps/cli/tauri.js'), 'build', '--target', 'x86_64-pc-windows-msvc', '--config', configPath, '--', '--locked'], { cwd: gui, stdio: 'inherit' });
const installers = (await readdir(path.join(target, 'bundle/nsis'))).filter(name => name.endsWith('.exe') && name.includes(`_${version}_`));
if (installers.length !== 1) throw new Error(`Expected one ${version} installer, found ${installers.length}`);
// Asset names stay version-free: the permanent link
// .../releases/latest/download/<asset> always serves the newest build.
const installer = 'FreeC-Tier_x64-setup.exe';
const installerSource = path.join(target, 'bundle/nsis', installers[0]);
await copyFile(installerSource, path.join(output, installer));
if (signed) {
  const signature = (await readFile(`${installerSource}.sig`, 'utf8')).trim();
  await writeFile(path.join(output, `${installer}.sig`), signature);
  await writeFile(path.join(output, 'latest.json'), JSON.stringify({
    version, notes: `FreeC Tier PREVIEW ${version}`, pub_date: new Date().toISOString(),
    platforms: { 'windows-x86_64': { signature, url: `https://github.com/${repo}/releases/latest/download/${installer}` } },
  }, null, 2));
}
const zip = path.join(output, 'FreeC-Tier_x64-portable.zip');
const packagingEnv = { ...process.env, FCT_PORTABLE: portable, FCT_ARCHIVE: zip };
// Same PowerShell 5.1 module-path cleanup as prepare-runtime.mjs.
delete packagingEnv.PSModulePath;
execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', 'Compress-Archive -LiteralPath $env:FCT_PORTABLE -DestinationPath $env:FCT_ARCHIVE -Force'], { stdio: 'inherit', env: packagingEnv });
const artifacts = [installer, path.basename(zip), ...(signed ? [`${installer}.sig`, 'latest.json'] : [])];
// Inner binaries get their own entries: antivirus false-positive submissions
// must reference the hash of the exact file Defender flags, not just the
// archive containing it.
const inner = ['freec-tier.exe', 'freec-runtime.exe', 'freec-service.exe'].map(name => `FreeC-Tier-release/${name}`);
const sums = await Promise.all(
  [...artifacts, ...inner].map(async name => {
    const file = name.startsWith('FreeC-Tier-release/') ? path.join(portable, path.basename(name)) : path.join(output, name);
    return `${createHash('sha256').update(await readFile(file)).digest('hex')}  ${name}`;
  }),
);
await writeFile(path.join(output, 'SHA256SUMS.txt'), sums.join('\n') + '\n');
console.log(`Release artifacts: ${output}`);
