# Reuses an earlier full release's signed natives when its natives key equals
# this build's (release.yml): the newest such release's natives.zip replaces
# the four natives in -Out, after check-signatures.ps1 passes them against
# -Signer. A release without natives-key.txt is skipped. Writes from=<tag> to
# $env:GITHUB_OUTPUT when it reuses; prints why when it doesn't.
#
#   powershell -ExecutionPolicy Bypass -File bin\reuse-natives.ps1 -Key <key> -Tag <this tag> -Out dist\natives -Signer <subject>
#
# -Gh/-GhArgs and -Checker replace gh and the signature check, for the tests
# (bin\reuse-natives.tests.ps1). Runs under Windows PowerShell 5.1 and
# PowerShell 7.
param(
  [Parameter(Mandatory = $true)][string]$Key,
  [string]$Tag,
  [Parameter(Mandatory = $true)][string]$Out,
  [string]$Signer = $env:EXPECTED_SIGNER,
  [string]$Gh = 'gh',
  [string[]]$GhArgs = @(),
  [string]$Checker = (Join-Path $PSScriptRoot 'check-signatures.ps1')
)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$contract = Get-Content -Raw -LiteralPath (Join-Path $root 'contract\artifacts.json') | ConvertFrom-Json
$names = @($contract.natives.x64.injector, $contract.natives.x64.shim, $contract.natives.x86.injector, $contract.natives.x86.shim)
$tmp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }

# Runs gh; returns @{ Exit; Out }. Its stderr is text to read, not an error:
# under 'Stop', Windows PowerShell makes any redirected stderr line of a
# native command a terminating error, so a release without natives-key.txt
# ("no assets match") would end the search instead of being skipped.
function Invoke-Gh([string[]]$Arguments) {
  $ErrorActionPreference = 'Continue'
  $global:LASTEXITCODE = 0
  $o = & $Gh @GhArgs @Arguments 2>&1 | ForEach-Object { "$_" }
  @{ Exit = $LASTEXITCODE; Out = @($o) }
}

$list = Invoke-Gh @('release', 'list', '--exclude-pre-releases', '--limit', '100', '--json', 'tagName', '--jq', '.[].tagName')
if ($list.Exit) { throw "gh release list failed (exit $($list.Exit)): $($list.Out -join ' ')" }
$tags = @($list.Out) | Where-Object { $_ -and $_ -ne $Tag }
foreach ($t in $tags) {
  $d = Join-Path $tmp "reuse-$t-$([guid]::NewGuid())"
  New-Item -ItemType Directory -Force $d | Out-Null
  $r = Invoke-Gh @('release', 'download', $t, '-p', 'natives-key.txt', '-D', $d, '--clobber')
  if ($r.Exit -or -not (Test-Path "$d\natives-key.txt")) { Write-Host "release ${t}: no natives-key.txt, skipped"; continue }
  if ((Get-Content -Raw "$d\natives-key.txt").Trim() -ne $Key) { continue }
  Write-Host "release $t has natives key ${Key}: reusing its natives"
  $r = Invoke-Gh @('release', 'download', $t, '-p', 'natives.zip', '-D', $d, '--clobber')
  if ($r.Exit) { throw "could not download $t's natives.zip: $($r.Out -join ' ')" }
  Expand-Archive -LiteralPath "$d\natives.zip" -DestinationPath "$d\natives" -Force
  $bad = @(& $Checker -Path ($names | ForEach-Object { Join-Path "$d\natives" $_ }) -Signer $Signer)
  if ($bad.Count) { throw "release $t's natives fail the signature check: $($bad -join '; ')" }
  foreach ($n in $names) { Copy-Item -LiteralPath (Join-Path "$d\natives" $n) -Destination (Join-Path $Out $n) -Force }
  if ($env:GITHUB_OUTPUT) { Add-Content -LiteralPath $env:GITHUB_OUTPUT -Encoding ascii -Value "from=$t" }
  exit 0
}
Write-Host "no earlier full release has natives key $Key"
exit 0
