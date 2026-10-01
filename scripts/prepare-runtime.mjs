// Stage native redistributables from Cargo's resolved Steamworks dependency.
// Wintun is downloaded only from its official host and verified before use.
import { execFileSync } from 'node:child_process';
import { mkdir, copyFile, writeFile, readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const profile = process.argv.includes('--release') ? 'release' : 'debug';
const outputIndex = process.argv.indexOf('--output');
if (outputIndex !== -1 && !process.argv[outputIndex + 1]) throw new Error('--output requires a directory');
const metadata = JSON.parse(execFileSync('cargo', ['metadata', '--format-version', '1', '--locked', '--filter-platform', 'x86_64-pc-windows-msvc'], { cwd: root, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 }));
const steam = metadata.packages.find(p => p.name === 'steamworks-sys');
if (!steam) throw new Error('Run cargo fetch first');
const output = outputIndex === -1 ? path.join(metadata.target_directory, profile) : path.resolve(process.argv[outputIndex + 1]);
await mkdir(output, { recursive: true });
const sdk = process.env.STEAM_SDK_LOCATION || path.join(path.dirname(steam.manifest_path), 'lib', 'steam');
await copyFile(path.join(sdk, 'redistributable_bin', 'win64', 'steam_api64.dll'), path.join(output, 'steam_api64.dll'));
console.log(`Steamworks library prepared automatically from ${sdk}`);
const cache = path.join(root, '.cache', 'wintun');
await mkdir(cache, { recursive: true });
const zip = path.join(cache, 'wintun.zip');
let bytes;
try { bytes = await readFile(zip); } catch (error) {
  if (error.code !== 'ENOENT') throw error;
  const url = 'https://www.wintun.net/builds/wintun-0.14.1.zip';
  console.log(`Downloading signed Wintun from ${url}`);
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(45000) });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    bytes = Buffer.from(await response.arrayBuffer());
  } catch (fetchError) {
    // Windows curl also supports standard HTTPS_PROXY settings. Keep TLS
    // verification enabled and fetch into memory until the hash is validated.
    console.log(`Node download failed (${fetchError.message}); retrying with Windows curl...`);
    try {
      bytes = execFileSync('curl.exe', ['--fail', '--location', '--silent', '--show-error', '--proto', '=https', '--proto-redir', '=https', '--connect-timeout', '20', '--max-time', '90', '--retry', '2', '--retry-max-time', '180', url], { timeout: 200000, maxBuffer: 16 * 1024 * 1024 });
    } catch (downloadError) {
      throw new Error(`Cannot download Wintun. Download ${url} manually to ${zip} and rerun the build. ${downloadError.message}`);
    }
  }
}
const checksum = createHash('sha256').update(bytes).digest('hex');
if (checksum !== '07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51') throw new Error('Wintun checksum mismatch');
await writeFile(zip, bytes);
execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', 'Expand-Archive -LiteralPath $env:FCT_ZIP -DestinationPath $env:FCT_EXTRACT -Force'], { env: { ...process.env, FCT_ZIP: zip, FCT_EXTRACT: cache }, stdio: 'inherit' });
await copyFile(path.join(cache, 'wintun', 'bin', 'amd64', 'wintun.dll'), path.join(output, 'wintun.dll'));
await copyFile(path.join(cache, 'wintun', 'LICENSE.txt'), path.join(output, 'WINTUN-LICENSE.txt'));

// The portable build carries the WebView2 Evergreen bootstrapper so a trimmed
// Windows can install the missing runtime in one click at first launch; the
// NSIS installer embeds its own copy. Microsoft re-signs the file over time,
// so Authenticode verification replaces the hash pinning used for Wintun.
const bootstrapperUrl = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703';
const bootstrapperCache = path.join(root, '.cache', 'webview2');
await mkdir(bootstrapperCache, { recursive: true });
const bootstrapperFile = path.join(bootstrapperCache, 'MicrosoftEdgeWebview2Setup.exe');
let bootstrapper;
try {
  bootstrapper = await readFile(bootstrapperFile);
} catch (error) {
  if (error.code !== 'ENOENT') throw error;
  console.log(`Downloading WebView2 bootstrapper from ${bootstrapperUrl}`);
  try {
    const response = await fetch(bootstrapperUrl, { signal: AbortSignal.timeout(45000) });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    bootstrapper = Buffer.from(await response.arrayBuffer());
  } catch (fetchError) {
    console.log(`Node download failed (${fetchError.message}); retrying with Windows curl...`);
    try {
      bootstrapper = execFileSync('curl.exe', ['--fail', '--location', '--silent', '--show-error', '--proto', '=https', '--proto-redir', '=https', '--connect-timeout', '20', '--max-time', '120', '--retry', '2', '--retry-max-time', '240', bootstrapperUrl], { timeout: 300000, maxBuffer: 16 * 1024 * 1024 });
    } catch (downloadError) {
      throw new Error(`Cannot download the WebView2 bootstrapper. Download it manually from ${bootstrapperUrl} to ${bootstrapperFile} and rerun the build. ${downloadError.message}`);
    }
  }
}
await writeFile(bootstrapperFile, bootstrapper);
const signature = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', `
$s = Get-AuthenticodeSignature -LiteralPath $env:FCT_BOOTSTRAP
if ($s.Status -ne 'Valid' -or $s.SignerCertificate.Subject -notlike '*Microsoft Corporation*') {
  throw "Untrusted WebView2 bootstrapper: $($s.Status) / $($s.SignerCertificate.Subject)"
}`], { env: { ...process.env, FCT_BOOTSTRAP: bootstrapperFile }, stdio: 'pipe' });
await copyFile(bootstrapperFile, path.join(output, 'MicrosoftEdgeWebview2Setup.exe'));
console.log(`WebView2 bootstrapper staged in ${output} (signed by Microsoft)`);
console.log(`Native libraries staged in ${output}`);
