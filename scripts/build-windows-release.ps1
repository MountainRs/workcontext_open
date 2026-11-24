# Run in an x64 MSVC developer PowerShell. Builds locally without installing.
param([switch]$Portable)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$target = Join-Path $root 'target\official-v01'
$previousTarget = $env:CARGO_TARGET_DIR
$previousIdentifier = $env:WORKCONTEXT_BUILD_IDENTIFIER
Push-Location $root
try {
    $config = Get-Content -Raw -Encoding UTF8 crates/desktop/tauri.conf.json | ConvertFrom-Json
    if ($config.identifier -ne 'com.local.workcontext' -or $env:TAURI_CONFIG) {
        throw 'Use the official desktop configuration without TAURI_CONFIG overrides.'
    }
    $env:CARGO_TARGET_DIR = $target
    $env:WORKCONTEXT_BUILD_IDENTIFIER = $config.identifier
    pnpm.cmd install --frozen-lockfile
    if ($LASTEXITCODE) { throw 'Frontend dependency installation failed.' }
    if ($Portable) { pnpm.cmd exec tauri build --no-bundle }
    else { pnpm.cmd exec tauri build --bundles nsis }
    if ($LASTEXITCODE) { throw 'Desktop packaging failed.' }
    cargo build --locked --release -p workcontext-cli
    if ($LASTEXITCODE) { throw 'CLI build failed.' }
    $installer = Join-Path $target "release\bundle\nsis\WorkContext_$($config.version)_x64-setup.exe"
    $cli = Join-Path $target 'release\workcontext.exe'
    & $cli --help | Out-Null
    if ($LASTEXITCODE) { throw 'CLI startup failed.' }
    $pair = Join-Path $target ('WorkContext-Windows-x64-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory $pair | Out-Null
    Copy-Item -LiteralPath $cli -Destination $pair
    if ($Portable) {
        Copy-Item -LiteralPath (Join-Path $target 'release\workcontext-desktop.exe') -Destination $pair
        foreach ($resource in $config.bundle.resources.PSObject.Properties) {
            $destination = Join-Path $pair $resource.Value
            New-Item -Force -ItemType Directory (Split-Path $destination -Parent) | Out-Null
            Copy-Item -LiteralPath (Join-Path 'crates/desktop' $resource.Name) -Destination $destination
        }
    } else {
        Copy-Item -LiteralPath $installer -Destination $pair
    }
    Write-Output $pair
} finally {
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:WORKCONTEXT_BUILD_IDENTIFIER = $previousIdentifier
    Pop-Location
}
