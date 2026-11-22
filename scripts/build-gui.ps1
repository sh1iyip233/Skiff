# Build the Tauri GUI (apps/skiff-gui).
#
# Three environment choices are required on a machine without Visual Studio; this script pins all
# of them so the build is reproducible:
#   1. MinGW binutils on PATH  -> provides `dlltool` for windows-sys raw-dylib import libraries.
#   2. RUSTUP_TOOLCHAIN=...gnu -> otherwise the Tauri CLI links with the MSVC target and fails on
#                                 missing kernel32.lib (no Windows SDK installed).
#   3. RUSTFLAGS=-C linker=rust-lld -> uses rustc's own linker instead of MinGW's gcc, which fails
#                                 when linking build scripts in this toolchain layout.
param(
  [switch]$Bundle
)

$root = Split-Path -Parent $PSScriptRoot
$gui = Join-Path $root 'apps\skiff-gui'
$mingwBin = Join-Path $root 'tools\mingw64\mingw64\bin'

if (Test-Path (Join-Path $mingwBin 'dlltool.exe')) {
  $env:PATH = "$mingwBin;$env:PATH"
  Write-Host "dlltool: $mingwBin\dlltool.exe"
} else {
  Write-Warning "dlltool not found under tools\mingw64; windows-sys based crates may fail to link"
}

$env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-gnu'
if (-not $env:RUSTFLAGS) { $env:RUSTFLAGS = '-C linker=rust-lld' }
Write-Host "toolchain: $env:RUSTUP_TOOLCHAIN"
Write-Host "RUSTFLAGS: $env:RUSTFLAGS"

if (-not (Test-Path (Join-Path $gui 'node_modules\.bin\tauri.cmd'))) {
  Write-Host "installing frontend tooling (only @tauri-apps/cli is required)"
  Push-Location $gui
  try { & npm install --no-audit --no-fund } finally { Pop-Location }
}

$tauriArgs = @('build')
if (-not $Bundle) { $tauriArgs += '--no-bundle' }

Push-Location $gui
try {
  & (Join-Path $gui 'node_modules\.bin\tauri.cmd') @tauriArgs
  $code = $LASTEXITCODE
} finally {
  Pop-Location
}

$exe = Join-Path $gui 'src-tauri\target\release\skiff-gui.exe'
if (Test-Path $exe) { Write-Host "built: $exe" }
exit $code
