# Tests bin/sign.ps1 against fake CodeSignTools, with no credentials of
# eSigner's: what it prints never holds a credential, a failed or unsigned
# signing leaves the file as it was, and -CheckOnly tells a working credential
# from a refused one.
#
#   powershell -ExecutionPolicy Bypass -File bin\sign.tests.ps1
#
# Exits non-zero, naming the failed cases, when any fails. Runs under Windows
# PowerShell 5.1 and PowerShell 7.
$ErrorActionPreference = 'Stop'

$sign = Join-Path $PSScriptRoot 'sign.ps1'
$work = Join-Path ([IO.Path]::GetTempPath()) ("aether-sign-tests-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $work | Out-Null
$failed = New-Object 'System.Collections.Generic.List[string]'

function Check([string]$Name, [bool]$Ok, [string]$Detail = '') {
  if ($Ok) { Write-Host "ok   $Name" } else { Write-Host "FAIL $Name $Detail"; $failed.Add($Name) }
}

# A fake tool: a PowerShell script run as `powershell -File <script> <args>`.
function New-FakeTool([string]$Name, [string]$Body) {
  $f = Join-Path $work "$Name.ps1"
  Set-Content -LiteralPath $f -Value $Body -Encoding UTF8
  $f
}

# Runs sign.ps1 in a child PowerShell with `$SignArgs` (a hashtable of its
# parameters), the fake tool, and the credentials set; returns @{ Exit; Out }.
# A script block, not -File: -File would hand -ToolArgs over as one string.
function Invoke-Sign([string]$Tool, [hashtable]$SignArgs, [switch]$NoPassword) {
  # a child's stderr is output to read, not an error to stop on
  $ErrorActionPreference = 'Continue'
  $env:ES_USERNAME = 'user@example.com'
  $env:ES_PASSWORD = if ($NoPassword) { '' } else { 'p%a&s^s' }
  $env:ES_CREDENTIAL_ID = 'cred-1234'
  $env:ES_TOTP_SECRET = 'totpsecretvalue'
  $toolArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Tool)
  $out = & powershell -NoProfile -ExecutionPolicy Bypass -Command {
    param($sign, $signArgs, $tool, $toolArgs)
    & $sign @signArgs -Tool $tool -ToolArgs $toolArgs
    exit $LASTEXITCODE
  } -args $sign, $SignArgs, (Join-Path $PSHOME 'powershell.exe'), $toolArgs 2>&1 | ForEach-Object { "$_" }
  @{ Exit = $LASTEXITCODE; Out = ($out -join "`n") }
}

$secrets = @('p%a&s^s', 'cred-1234', 'totpsecretvalue', 'user@example.com')
function Test-NoSecrets([string]$Name, [string]$Out) {
  $leaked = @($secrets | Where-Object { $Out.Contains($_) })
  Check "$Name prints no credential" ($leaked.Count -eq 0) ("leaked: " + ($leaked -join ', '))
}

try {
  # an unsigned PE to sign: a copy of a system program without a signature
  # is hard to come by, so any file stands in (the fakes never read it)
  $exe = Join-Path $work 'x.exe'
  [IO.File]::WriteAllBytes($exe, [byte[]](1..64))
  $before = (Get-FileHash -LiteralPath $exe).Hash

  # 1. The tool fails, echoing its arguments (CodeSignTool does on some errors)
  $echo = New-FakeTool 'echo-fail' 'Write-Output ("tool got: " + ($args -join " ")); exit 1'
  $r = Invoke-Sign $echo @{ Path = $exe }
  Check 'a failed signing ran the tool' ($r.Out.Contains('tool got: sign ')) $r.Out
  Check 'a failed signing exits non-zero' ($r.Exit -ne 0) "exit $($r.Exit)"
  Check 'a failed signing says [redacted]' ($r.Out.Contains('[redacted]')) $r.Out
  Test-NoSecrets 'a failed signing' $r.Out
  Check 'a failed signing leaves the file' ((Get-FileHash -LiteralPath $exe).Hash -eq $before)
  # whole: one argument, `-password=` and all of it, so it was redacted whole
  Check 'the password reaches the tool whole' ($r.Out -match '-password=\[redacted\] ') $r.Out

  # 2. The tool "succeeds" but writes an unsigned copy
  $copy = New-FakeTool 'unsigned-copy' @'
$dir = ($args | Where-Object { $_ -like '-output_dir_path=*' }) -replace '^-output_dir_path=', ''
$in = ($args | Where-Object { $_ -like '-input_file_path=*' }) -replace '^-input_file_path=', ''
Copy-Item -LiteralPath $in -Destination (Join-Path $dir (Split-Path -Leaf $in))
Write-Output 'copied'
exit 0
'@
  $r = Invoke-Sign $copy @{ Path = $exe }
  Check 'an unsigned copy ran the tool' ($r.Out.Contains('copied')) $r.Out
  Check 'an unsigned copy exits non-zero' ($r.Exit -ne 0) "exit $($r.Exit)"
  Check 'an unsigned copy names the file' ($r.Out.Contains('did not produce a signed x.exe')) $r.Out
  Test-NoSecrets 'an unsigned copy' $r.Out
  Check 'an unsigned copy leaves the file' ((Get-FileHash -LiteralPath $exe).Hash -eq $before)

  # 3. -CheckOnly with a credential that works
  $info = New-FakeTool 'info-ok' @'
Write-Output 'EVCS Certificate Subject Information:'
Write-Output '- Subject DN: CN=Example Ltd, O=Example Ltd, C=GB'
exit 0
'@
  $r = Invoke-Sign $info @{ CheckOnly = $true }
  Check '-CheckOnly passes a working credential' ($r.Exit -eq 0) $r.Out
  Check '-CheckOnly prints the subject' ($r.Out.Contains('Subject DN: CN=Example Ltd')) $r.Out

  # 4. -CheckOnly with a refused one: the tool exits 0 and prints an error
  $refused = New-FakeTool 'info-refused' 'Write-Output ("Error: invalid credentials for " + ($args -join " ")); exit 0'
  $r = Invoke-Sign $refused @{ CheckOnly = $true }
  Check '-CheckOnly ran the tool' ($r.Out.Contains('Error: invalid credentials for credential_info')) $r.Out
  Check '-CheckOnly fails a refused credential' ($r.Exit -ne 0) $r.Out
  Test-NoSecrets '-CheckOnly' $r.Out

  # 5. No credentials: refuses before running anything
  $r = Invoke-Sign $echo @{ Path = $exe } -NoPassword
  Check 'missing credentials exit non-zero' ($r.Exit -ne 0) "exit $($r.Exit)"
  Check 'missing credentials run no tool' (-not $r.Out.Contains('tool got')) $r.Out
  Check 'missing credentials say so' ($r.Out.Contains('credentials missing')) $r.Out
} finally {
  foreach ($n in 'ES_USERNAME', 'ES_PASSWORD', 'ES_CREDENTIAL_ID', 'ES_TOTP_SECRET') {
    Remove-Item "Env:$n" -ErrorAction SilentlyContinue
  }
  Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failed.Count) {
  Write-Host "$($failed.Count) failed: $($failed -join '; ')"
  exit 1
}
Write-Host 'all passed'
