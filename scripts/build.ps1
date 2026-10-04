# Windows PowerShell 5.1. No machine-wide environment changes or elevation.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'
$profile = 'release'

function Invoke-Checked {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed (exit code $LASTEXITCODE)." }
}

Push-Location -LiteralPath $root
try {
    foreach ($argument in $args) {
        switch ($argument) {
            '--debug' { $profile = 'debug' }
            '--installer' { $installer = $true }
            '--no-pause' { }
            default { throw "Unknown option: $argument. Usage: build.bat [--debug] [--no-pause]" }
        }
    }
    foreach ($tool in @('node.exe', 'npm.cmd', 'cargo.exe', 'rustc.exe')) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            throw "Missing $tool. Install Node.js (22.12+ recommended), Rust MSVC and Visual Studio Build Tools with Desktop development with C++."
        }
    }
    $nodeVersionText = & node.exe --version
    if ($LASTEXITCODE -ne 0) { throw 'Cannot detect Node.js version.' }
    $nodeVersion = [version]($nodeVersionText.Trim().TrimStart('v'))
    if (-not ($nodeVersion.Major -gt 22 -or ($nodeVersion.Major -eq 22 -and $nodeVersion.Minor -ge 12) -or ($nodeVersion.Major -eq 20 -and $nodeVersion.Minor -ge 19))) {
        throw 'Node.js 20.19+ or 22.12+ is required.'
    }

    # Release version comes from FREECTIER_VERSION or the git tag, falling back to
    # package.json. It is injected through TAURI_CONFIG so the EXE reports it.
    $version = $env:FREECTIER_VERSION
    if (-not $version -and $env:GITHUB_REF_TYPE -eq 'tag') { $version = $env:GITHUB_REF_NAME }
    if (-not $version) { $version = (Get-Content -Raw (Join-Path $root 'client\gui\package.json') | ConvertFrom-Json).version }
    $version = $version -replace '^v', ''
    if ($version -notmatch '^\d+\.\d+\.\d+([-+][\w.-]+)?$') { throw "Invalid release version '$version'. Use semver such as 0.2.1 or 0.2.1-preview." }
    Write-Host "Version: $version"

    Write-Host "[1/5] Fetching locked Rust dependencies, including Steamworks SDK libraries..."
    # Use a predictable local target path, independently of user Cargo settings.
    $env:CARGO_TARGET_DIR = Join-Path $root 'target'
    if ($env:STEAM_SDK_LOCATION) {
        $env:STEAM_SDK_LOCATION = (Resolve-Path -LiteralPath $env:STEAM_SDK_LOCATION).Path
        foreach ($file in @('steam_api64.dll', 'steam_api64.lib')) {
            $sdkFile = Join-Path $env:STEAM_SDK_LOCATION "redistributable_bin\win64\$file"
            if (-not (Test-Path -LiteralPath $sdkFile -PathType Leaf)) { throw "Custom STEAM_SDK_LOCATION is missing $sdkFile. Unset it to use automatic Steamworks dependencies." }
        }
        Write-Host "Custom SDK: $env:STEAM_SDK_LOCATION"
    } else {
        Write-Host 'Steamworks: automatic SDK libraries from steamworks-sys (Cargo.lock).'
    }
    Invoke-Checked 'cargo.exe' @('fetch', '--locked', '--target', $target)

    Write-Host '[2/5] Preparing Steamworks and signed Wintun DLLs...'
    $nativeOutput = Join-Path $root "target\$target\$profile"
    Invoke-Checked 'node.exe' @('scripts/prepare-runtime.mjs', '--output', $nativeOutput)

    Write-Host '[3/5] Installing and building frontend...'
    Invoke-Checked 'npm.cmd' @('--prefix', 'client/gui', 'ci', '--no-audit', '--no-fund')
    Invoke-Checked 'npm.cmd' @('--prefix', 'client/gui', 'run', 'build')

    Write-Host "[4/5] Building Windows x64 desktop and diagnostic client ($profile)..."
    $env:TAURI_CONFIG = (@{ version = $version } | ConvertTo-Json -Compress)
    $cargoArgs = @('build', '--locked', '--target', $target, '-p', 'freec-tier', '-p', 'freec-runtime', '-p', 'freec-service')
    if ($profile -eq 'release') { $cargoArgs += '--release' }
    Invoke-Checked 'cargo.exe' $cargoArgs

    Write-Host '[5/5] Assembling launch folder...'
    $destination = Join-Path $root "dist\FreeC-Tier-$profile"
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    foreach ($file in @('freec-tier.exe', 'freec-runtime.exe', 'freec-service.exe', 'steam_api64.dll', 'wintun.dll', 'WINTUN-LICENSE.txt', 'MicrosoftEdgeWebview2Setup.exe')) {
        Copy-Item -LiteralPath (Join-Path $nativeOutput $file) -Destination $destination -Force
    }
    Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination $destination -Force
    Write-Host "`nBuild complete: $destination"
    Write-Host "Start: $(Join-Path $destination 'freec-tier.exe')"
    if ($installer) {
        if ($profile -ne 'release') { throw '--installer requires the release profile (drop --debug).' }
        Write-Host '[6/5] Building the NSIS installer package...'
        Invoke-Checked 'node.exe' @('scripts/release.mjs')
        Write-Host "Installer package: $(Join-Path $root 'dist\releases')"
    }
    Write-Host 'Steam and WebView2 Runtime must be installed. The desktop EXE requests administrator rights through Windows UAC on launch.'
} catch {
    [Console]::Error.WriteLine("Build error: $_")
    exit 1
} finally {
    Pop-Location
}
