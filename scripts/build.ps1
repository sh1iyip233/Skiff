# Build helper: picks a Rust toolchain that can actually link on this machine.
#
# Background: the MSVC target needs Visual Studio's link.exe, which is often absent. The GNU target
# is self-contained, but crates such as windows-sys >= 0.60 need `dlltool` (raw-dylib import
# libraries) and MinGW's gcc link step can fail for build scripts. The combination that works here
# is: MinGW's binutils on PATH + rust-lld as the linker.
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

$buildArgs = @("+$Toolchain", 'build', "--$Profile")
Write-Host "toolchain: $Toolchain"
& $cargo @buildArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$exe = Join-Path $root "target\$Profile\skiff.exe"
if (Test-Path $exe) {
  Write-Host "built: $exe"
} else {
  $alt = Join-Path $root "target\${Toolchain}\$Profile\skiff.exe"
  if (Test-Path $alt) { Write-Host "built: $alt" }
}
