# Run a child process with file-redirected stdio and report exit code + output.
#
# This environment's sandbox forbids the named-pipe stdio that PowerShell normally uses to
# capture native command output, so every build/test invocation goes through this helper.
param(
  [Parameter(Mandatory = $true)][string]$Exe,
  [string[]]$Args = @(),
  [string]$WorkDir = '',
  [int]$Tail = 120
)

$tag = [guid]::NewGuid().ToString('N').Substring(0, 8)
$out = Join-Path $env:TEMP "px-$tag-out.txt"
$err = Join-Path $env:TEMP "px-$tag-err.txt"

$params = @{
  FilePath               = $Exe
  ArgumentList           = $Args
  NoNewWindow            = $true
  Wait                   = $true
  PassThru               = $true
  RedirectStandardOutput = $out
  RedirectStandardError  = $err
}
if ($WorkDir) { $params.WorkingDirectory = $WorkDir }

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$p = Start-Process @params -ErrorAction Stop
$sw.Stop()
$code = $p.ExitCode

$so = if (Test-Path $out) { Get-Content -LiteralPath $out -Raw -ErrorAction SilentlyContinue } else { '' }
$se = if (Test-Path $err) { Get-Content -LiteralPath $err -Raw -ErrorAction SilentlyContinue } else { '' }
Remove-Item -LiteralPath $out, $err -Force -ErrorAction SilentlyContinue

"===== exit=$code  elapsed=$([int]$sw.Elapsed.TotalSeconds)s ====="
if ($se) {
  "----- stderr (tail $Tail) -----"
  ($se -split "`r?`n" | Select-Object -Last $Tail) -join "`n"
}
if ($so) {
  "----- stdout (tail $Tail) -----"
  ($so -split "`r?`n" | Select-Object -Last $Tail) -join "`n"
}
exit $code
