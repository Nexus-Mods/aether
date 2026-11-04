# Signs files with SSL.com's CodeSignTool (eSigner), or checks the signing
# credential.
#
#   powershell -ExecutionPolicy Bypass -File bin\sign.ps1 -Path a.exe, b.dll [-ToolDir CodeSignTool]
#   powershell -ExecutionPolicy Bypass -File bin\sign.ps1 -CheckOnly
#
# The credentials are ES_USERNAME, ES_PASSWORD, ES_CREDENTIAL_ID and
# ES_TOTP_SECRET; it refuses before running anything when one is missing.
# bin\download-codesigntool.ps1 puts CodeSignTool in CodeSignTool\ at the
# repository root, -ToolDir's default.
#
# Each file is signed into a folder of its own under a temporary folder, the
# copy is checked to carry a signature, and only then does it replace the
# original: a failed or unsigned signing leaves the file as it was. A file
# that already carries a signature is skipped. -CheckOnly asks eSigner about
# the credential (credential_info) and fails unless its answer is the
# certificate's: the tool exits 0 on most failures.
#
# The tool runs as its bundled JDK on the jar, not through CodeSignTool.bat:
# cmd would expand %, & and ^ in a password. Everything printed has the four
# credential values replaced by [redacted], the longest first (one may hold
# another). -Tool and -ToolArgs replace the JDK and jar, for the tests
# (bin\sign.tests.ps1).
#
# Runs under Windows PowerShell 5.1 and PowerShell 7.
param(
  [string[]]$Path,
  [switch]$CheckOnly,
  [string]$ToolDir,
  [string]$Tool,
  [string[]]$ToolArgs
)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
if (-not $ToolDir) { $ToolDir = Join-Path $root 'CodeSignTool' }

$names = [ordered]@{
  username = 'ES_USERNAME'; password = 'ES_PASSWORD'
  credential = 'ES_CREDENTIAL_ID'; totp = 'ES_TOTP_SECRET'
}
$cred = @{}
foreach ($k in $names.Keys) {
  $v = [Environment]::GetEnvironmentVariable($names[$k])
  if ([string]::IsNullOrWhiteSpace($v)) {
    Write-Host "sign: eSigner credentials missing ($($names.Values -join ', '))"
    exit 2
  }
  $cred[$k] = $v
}
$secrets = @($cred.Values | Sort-Object -Property Length -Descending)

function Hide([string]$s) {
  foreach ($v in $secrets) { $s = $s.Replace($v, '[redacted]') }
  $s
}

# The command that runs the tool: the bundled JDK on the jar, or the tests'.
if ($Tool) {
  $exe = $Tool
  $prefix = @($ToolArgs)
} else {
  $exe = Join-Path $ToolDir 'jdk-11.0.2\bin\java.exe'
  $jar = Get-ChildItem -LiteralPath (Join-Path $ToolDir 'jar') -Filter 'code_sign_tool-*.jar' -ErrorAction SilentlyContinue |
    Select-Object -First 1
  if (-not (Test-Path -LiteralPath $exe -PathType Leaf) -or -not $jar) {
    Write-Host "sign: CodeSignTool is not in $ToolDir (run powershell -ExecutionPolicy Bypass -File bin\download-codesigntool.ps1)"
    exit 2
  }
  $env:CODE_SIGN_TOOL_PATH = $ToolDir
  $prefix = @('-Dfile.encoding=UTF-8', '-Dsun.stdout.encoding=UTF-8', '-Dsun.stderr.encoding=UTF-8', '-jar', $jar.FullName)
}

# Runs the tool with `$Arguments`; returns @{ Exit; Out } (output and errors
# merged, redacted). Its input is closed: nothing may wait on a prompt.
function Invoke-Tool([string[]]$Arguments) {
  $ErrorActionPreference = 'Continue'
  Push-Location $ToolDir -ErrorAction SilentlyContinue
  try {
    $global:LASTEXITCODE = 0
    $out = $null | & $exe @prefix @Arguments 2>&1 | ForEach-Object { "$_" }
    @{ Exit = $LASTEXITCODE; Out = (Hide (($out | ForEach-Object { $_ }) -join "`n")).Trim() }
  } finally {
    Pop-Location -ErrorAction SilentlyContinue
  }
}

function Test-Signed([string]$File) {
  $null -ne (Get-AuthenticodeSignature -LiteralPath $File).SignerCertificate
}

if ($CheckOnly) {
  $r = Invoke-Tool @('credential_info', "-username=$($cred.username)", "-password=$($cred.password)", "-credential_id=$($cred.credential)")
  $ok = ($r.Exit -eq 0) -and
        ($r.Out -match '(?m)^EVCS Certificate Subject Information:[ \t]*\r?\n-\s*Subject DN: \S') -and
        -not ($r.Out -match '(?m)^\s*(Error|\S+(Exception|Error)\b)')
  if (-not $ok) {
    Write-Host "sign: eSigner credentials don't work (exit $($r.Exit)):"
    Write-Host $r.Out
    exit 1
  }
  Write-Host $r.Out
  exit 0
}

if (-not $Path) { Write-Host 'sign: -Path names no files'; exit 2 }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("aether-sign-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
  $i = 0
  foreach ($p in $Path) {
    $in = (Resolve-Path -LiteralPath $p).Path
    $name = Split-Path -Leaf $in
    if (Test-Signed $in) { Write-Host "  already signed: $in"; continue }
    Write-Host "  signing: $in"
    # a folder per file: names repeat across folders
    $dir = Join-Path $tmp ($i++)
    New-Item -ItemType Directory -Force $dir | Out-Null
    $r = Invoke-Tool @('sign', "-input_file_path=$in", "-output_dir_path=$dir",
      "-credential_id=$($cred.credential)", "-username=$($cred.username)",
      "-password=$($cred.password)", "-totp_secret=$($cred.totp)")
    $out = Join-Path $dir $name
    if ($r.Exit -ne 0 -or -not (Test-Path -LiteralPath $out -PathType Leaf) -or -not (Test-Signed $out)) {
      Write-Host "sign: CodeSignTool did not produce a signed $name (exit $($r.Exit))"
      if ($r.Out) { Write-Host $r.Out }
      exit 1
    }
    # beside the original first, then over it: a copy across volumes is not a
    # replacement, a move on one volume is
    $beside = "$in.signed"
    Copy-Item -LiteralPath $out -Destination $beside -Force
    Move-Item -LiteralPath $beside -Destination $in -Force
  }
} finally {
  Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
