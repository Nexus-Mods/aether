# Tests bin/reuse-natives.ps1 against a fake gh and a fake signature check:
# a release without natives-key.txt (gh fails, writing to stderr) is skipped
# rather than stopping the search, a matching release's natives are reused
# only when they pass the signature check, and no match reuses nothing.
#
#   powershell -ExecutionPolicy Bypass -File bin\reuse-natives.tests.ps1
#
# Exits non-zero, naming the failed cases, when any fails.
$ErrorActionPreference = 'Stop'

$reuse = Join-Path $PSScriptRoot 'reuse-natives.ps1'
$work = Join-Path ([IO.Path]::GetTempPath()) ("aether-reuse-tests-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $work | Out-Null
$failed = New-Object 'System.Collections.Generic.List[string]'
function Check([string]$Name, [bool]$Ok, [string]$Detail = '') {
  if ($Ok) { Write-Host "ok   $Name" } else { Write-Host "FAIL $Name $Detail"; $failed.Add($Name) }
}

$names = @('aether-injector.exe', 'aether_shim.dll', 'aether-injector32.exe', 'aether_shim32.dll')

# The releases the fake gh knows: each a folder of its assets.
$releases = Join-Path $work 'releases'
function New-Release([string]$Tag, [string]$Key, [switch]$NoKey) {
  $d = Join-Path $releases $Tag
  New-Item -ItemType Directory -Force $d | Out-Null
  if (-not $NoKey) { Set-Content -LiteralPath (Join-Path $d 'natives-key.txt') -Value $Key -Encoding ascii }
  $n = Join-Path $work "n-$Tag"
  New-Item -ItemType Directory -Force $n | Out-Null
  foreach ($f in $names) { Set-Content -LiteralPath (Join-Path $n $f) -Value "$Tag $f" -Encoding ascii }
  Compress-Archive -Path (Join-Path $n '*') -DestinationPath (Join-Path $d 'natives.zip') -Force
}

# gh release list / gh release download, from $releases, newest first by the
# order in list.txt; a missing asset is gh's "no assets match" on stderr, exit 1
$gh = Join-Path $work 'gh.ps1'
Set-Content -LiteralPath $gh -Encoding UTF8 -Value @"
`$releases = '$releases'
if (`$args[0] -eq 'release' -and `$args[1] -eq 'list') {
  Get-Content (Join-Path `$releases 'list.txt'); exit 0
}
if (`$args[0] -eq 'release' -and `$args[1] -eq 'download') {
  `$tag = `$args[2]; `$pattern = `$args[(`$args.IndexOf('-p') + 1)]; `$dir = `$args[(`$args.IndexOf('-D') + 1)]
  `$f = Join-Path (Join-Path `$releases `$tag) `$pattern
  if (-not (Test-Path -LiteralPath `$f)) { [Console]::Error.WriteLine('no assets match the file pattern'); exit 1 }
  Copy-Item -LiteralPath `$f -Destination `$dir -Force; exit 0
}
[Console]::Error.WriteLine("fake gh: unexpected `$args"); exit 9
"@

# a signature check that passes, and one that fails every file
$pass = Join-Path $work 'check-pass.ps1'
Set-Content -LiteralPath $pass -Encoding UTF8 -Value 'param([string[]]$Path, [string]$Signer) Write-Host "checked $($Path.Count) as $Signer"'
$fail = Join-Path $work 'check-fail.ps1'
Set-Content -LiteralPath $fail -Encoding UTF8 -Value 'param([string[]]$Path, [string]$Signer) foreach ($p in $Path) { "$p (NotSigned)" }'

function Invoke-Reuse([string]$Key, [string]$Checker, [string]$Out) {
  $ErrorActionPreference = 'Continue'
  New-Item -ItemType Directory -Force $Out | Out-Null
  foreach ($f in $names) { Set-Content -LiteralPath (Join-Path $Out $f) -Value 'built' -Encoding ascii }
  $ghOut = Join-Path $work ("out-" + [guid]::NewGuid())
  $env:GITHUB_OUTPUT = $ghOut
  $o = & powershell -NoProfile -ExecutionPolicy Bypass -Command {
    param($reuse, $key, $out, $gh, $checker)
    & $reuse -Key $key -Tag 'v9.9.9' -Out $out -Signer 'CN=Test' `
      -Gh (Join-Path $PSHOME 'powershell.exe') -GhArgs @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $gh) `
      -Checker $checker
    exit $LASTEXITCODE
  } -args $reuse, $Key, $Out, $gh, $Checker 2>&1 | ForEach-Object { "$_" }
  $from = ''
  if (Test-Path $ghOut) { $from = ((Get-Content $ghOut) | Where-Object { $_ -like 'from=*' }) -replace '^from=', '' }
  @{ Exit = $LASTEXITCODE; Out = ($o -join "`n"); From = $from }
}

try {
  # newest first: v0.3.0 has no natives-key.txt, v0.2.0 another key, v0.1.0 ours
  New-Release 'v0.3.0' 'x' -NoKey
  New-Release 'v0.2.0' 'otherkey'
  New-Release 'v0.1.0' 'ourkey'
  Set-Content -LiteralPath (Join-Path $releases 'list.txt') -Value @('v0.3.0', 'v0.2.0', 'v0.1.0') -Encoding ascii

  $out = Join-Path $work 'dist1'
  $r = Invoke-Reuse 'ourkey' $pass $out
  Check 'a release without natives-key.txt does not stop the search' ($r.Exit -eq 0) $r.Out
  Check 'the matching release is reused' ($r.From -eq 'v0.1.0') "from='$($r.From)' $($r.Out)"
  Check 'its natives replace the built ones' ((Get-Content -Raw (Join-Path $out 'aether_shim.dll')).Trim() -eq 'v0.1.0 aether_shim.dll')
  Check 'the check is told the signer' ($r.Out.Contains('checked 4 as CN=Test')) $r.Out

  $out = Join-Path $work 'dist2'
  $r = Invoke-Reuse 'ourkey' $fail $out
  Check 'natives that fail the check stop the release' ($r.Exit -ne 0) $r.Out
  Check 'and are not reused' ((Get-Content -Raw (Join-Path $out 'aether_shim.dll')).Trim() -eq 'built')
  Check 'no reuse is reported' ($r.From -eq '')

  $out = Join-Path $work 'dist3'
  $r = Invoke-Reuse 'nokey' $pass $out
  Check 'no match exits 0' ($r.Exit -eq 0) $r.Out
  Check 'no match reuses nothing' ($r.From -eq '' -and (Get-Content -Raw (Join-Path $out 'aether-injector.exe')).Trim() -eq 'built')

  # the last release asked about has no natives-key.txt: gh's failure must
  # not become the script's exit code
  Set-Content -LiteralPath (Join-Path $releases 'list.txt') -Value @('v0.2.0', 'v0.3.0') -Encoding ascii
  $r = Invoke-Reuse 'nokey' $pass (Join-Path $work 'dist4')
  Check 'no match after a keyless release exits 0' ($r.Exit -eq 0) "exit $($r.Exit) $($r.Out)"
} finally {
  Remove-Item Env:GITHUB_OUTPUT -ErrorAction SilentlyContinue
  Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failed.Count) { Write-Host "$($failed.Count) failed: $($failed -join '; ')"; exit 1 }
Write-Host 'all passed'
exit 0
