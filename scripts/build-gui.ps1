# Build the Slint desktop client (apps/skiff-slint) -> target/<profile>/skiff-ui.exe.
#
# Same toolchain selection as build.ps1: MinGW's binutils provide `dlltool` for windows-sys
# raw-dylib import libraries, and rust-lld is used as the linker so the GNU target can link
# without Visual Studio.
param(
  [ValidateSet('debug', 'release')][string]$Profile = 'release',
  [string]$Toolchain = ''
)

$root = Split-Path -Parent $PSScriptRoot
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) { $cargo = 'cargo' }

# 1) Provide dlltool from a locally unpacked MinGW toolchain, if present.
$mingwBin = Join-Path $root 'tools\mingw64\mingw64\bin'
$dlltool = Join-Path $mingwBin 'dlltool.exe'
if (Test-Path $dlltool) {
  $env:PATH = "$mingwBin;$env:PATH"
  Write-Host "dlltool: $dlltool"
}

if (-not $Toolchain) {
  $link = Get-Command link.exe -ErrorAction SilentlyContinue
  if ($link) {
    $Toolchain = 'stable-x86_64-pc-windows-msvc'
  } else {
    $Toolchain = 'stable-x86_64-pc-windows-gnu'
  }
}

$isGnu = $Toolchain -like '*gnu*'
if ($isGnu -and (Test-Path $dlltool)) {
  # Keep rustc's own linker so MinGW's gcc is only used for `dlltool`.
  if (-not $env:RUSTFLAGS) { $env:RUSTFLAGS = '-C linker=rust-lld' }
  Write-Host "RUSTFLAGS: $env:RUSTFLAGS"
}

$buildArgs = @("+$Toolchain", 'build', "--$Profile", '-p', 'skiff-slint')
Write-Host "toolchain: $Toolchain"
& $cargo @buildArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$exe = Join-Path $root "target\$Profile\skiff-ui.exe"
if (Test-Path $exe) {
  Write-Host "built: $exe"
} else {
  $alt = Join-Path $root "target\${Toolchain}\$Profile\skiff-ui.exe"
  if (Test-Path $alt) { Write-Host "built: $alt" }
}
