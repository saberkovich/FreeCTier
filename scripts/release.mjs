// Run build.bat --no-pause first. This builds an NSIS installer; it never publishes.
import { readFile, writeFile, readdir, copyFile, mkdir } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const gui = path.join(root, 'client/gui');
const version = JSON.parse(await readFile(path.join(gui, 'package.json'), 'utf8')).version;
const signed = process.argv.includes('--signed');
const repo = process.env.FREECTIER_GITHUB_REPOSITORY;
if (signed && (!/^[\w.-]+\/[\w.-]+$/.test(repo || '') || !process.env.FREECTIER_UPDATER_PUBLIC_KEY || !process.env.TAURI_SIGNING_PRIVATE_KEY)) {
  throw new Error('Signed release needs FREECTIER_GITHUB_REPOSITORY, FREECTIER_UPDATER_PUBLIC_KEY and TAURI_SIGNING_PRIVATE_KEY.');
}
if (process.env.GITHUB_REF_TYPE === 'tag' && process.env.GITHUB_REF_NAME !== `v${version}`) throw new Error('Git tag must match package version');
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
  bundle: {
    active: true, targets: ['nsis'], createUpdaterArtifacts: signed, icon: [iconPath],
    resources: Object.fromEntries(['steam_api64.dll', 'wintun.dll', 'WINTUN-LICENSE.txt'].map(name => [path.join(portable, name), name])),
    windows: { nsis: { installMode: 'perMachine', languages: ['Russian', 'English'], displayLanguageSelector: true } },
  },
};
const configPath = path.join(root, '.cache/release.conf.json');
await writeFile(configPath, JSON.stringify(config, null, 2));
execFileSync(process.execPath, [path.join(gui, 'node_modules/@tauri-apps/cli/tauri.js'), 'build', '--target', 'x86_64-pc-windows-msvc', '--config', configPath, '--', '--locked'], { cwd: gui, stdio: 'inherit' });
const installers = (await readdir(path.join(target, 'bundle/nsis'))).filter(name => name.endsWith('.exe') && name.includes(`_${version}_`));
if (installers.length !== 1) throw new Error(`Expected one ${version} installer, found ${installers.length}`);
const installer = `FreeC-Tier_${version}_x64-setup.exe`;
const installerSource = path.join(target, 'bundle/nsis', installers[0]);
await copyFile(installerSource, path.join(output, installer));
if (signed) {
  const signature = (await readFile(`${installerSource}.sig`, 'utf8')).trim();
  await writeFile(path.join(output, `${installer}.sig`), signature);
  await writeFile(path.join(output, 'latest.json'), JSON.stringify({
    version, notes: `FreeC Tier PREVIEW ${version}`, pub_date: new Date().toISOString(),
    platforms: { 'windows-x86_64': { signature, url: `https://github.com/${repo}/releases/download/v${version}/${installer}` } },
  }, null, 2));
}
const zip = path.join(output, `FreeC-Tier_${version}_x64-portable.zip`);
execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', 'Compress-Archive -LiteralPath $env:FCT_PORTABLE -DestinationPath $env:FCT_ARCHIVE -Force'], { stdio: 'inherit', env: { ...process.env, FCT_PORTABLE: portable, FCT_ARCHIVE: zip } });
const artifacts = [installer, path.basename(zip), ...(signed ? [`${installer}.sig`, 'latest.json'] : [])];
const sums = await Promise.all(artifacts.map(async name => `${createHash('sha256').update(await readFile(path.join(output, name))).digest('hex')}  ${name}`));
await writeFile(path.join(output, 'SHA256SUMS.txt'), sums.join('\n') + '\n');
console.log(`Release artifacts: ${output}`);
