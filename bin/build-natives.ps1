# Builds Aether's Windows natives: the x64 injector and shim DLL and the i686
# pair for 32-bit games, with the C runtime linked in (+crt-static), staged
# under the names a director looks for.
#
#   bin/build-natives.ps1 [-Fixtures] [-Out <dir>] [-RequireDumpbin]
#
#   -Out             where the four are staged (default target/natives)
#   -Fixtures        also build the test fixtures (aether-fixtures' programs,
#                    aether-fixture-vproxy and -staticimp, and aether-inject's
#                    aether-probe) and the host-target injector and
#                    shim into target/release, where the tests (and a
#                    director's) look
#   -RequireDumpbin  fail, rather than warn, when dumpbin (Visual Studio's MSVC
#                    tools) isn't there for the VC++ runtime import check; on
#                    by default when $env:CI is true or 1 (CI must not skip it)
#
# The C runtime is linked in because the shim loads inside the game, where
# Windows doesn't look beside it for vcruntime140.dll, and a PC that never
# installed the game through its store may have no VC++ runtime.
param(
  [switch]$Fixtures,
  [string]$Out,
  [switch]$RequireDumpbin
)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
# The names the natives are staged under, and the fixture files: one list,
# which a release ships (contract.zip) and bin/build-windows reads too
$contract = Get-Content -Raw -LiteralPath (Join-Path $root 'contract\artifacts.json') | ConvertFrom-Json
if (-not $Out) { $Out = Join-Path $root 'target\natives' }
# a relative -Out is the caller's, not the root's (the script changes into the root below)
if (-not [IO.Path]::IsPathRooted($Out)) { $Out = Join-Path (Get-Location).Path $Out }
$Out = [IO.Path]::GetFullPath($Out)

# Runs a native command; any non-zero exit fails, naming the step.
function Invoke-Step([string]$Name, [scriptblock]$Body) {
  $global:LASTEXITCODE = 0
  Write-Host "== $Name"
  & $Body
  if ($LASTEXITCODE) { throw "build-natives: step '$Name' failed (exit $LASTEXITCODE)" }
}

Push-Location $root
try {
  Invoke-Step 'rustup target add i686' { rustup target add i686-pc-windows-msvc }

  $old = $env:RUSTFLAGS
  $env:RUSTFLAGS = '-C target-feature=+crt-static'
  try {
    Invoke-Step 'build x64 injector and shim' {
      cargo build --release --target x86_64-pc-windows-msvc -p aether-inject -p aether-shim-dll
    }
    Invoke-Step 'build i686 injector' {
      cargo build --release --target i686-pc-windows-msvc -p aether-inject --bin aether-injector
    }
    Invoke-Step 'build i686 shim' {
      cargo build --release --target i686-pc-windows-msvc -p aether-shim-dll
    }
  } finally {
    $env:RUSTFLAGS = $old
  }

  # staged name (contract/artifacts.json) -> what the build produced
  $t = Join-Path $root 'target'
  $natives = [ordered]@{}
  $natives[$contract.natives.x64.injector] = "$t\x86_64-pc-windows-msvc\release\aether-injector.exe"
  $natives[$contract.natives.x64.shim] = "$t\x86_64-pc-windows-msvc\release\aether_shim_dll.dll"
  $natives[$contract.natives.x86.injector] = "$t\i686-pc-windows-msvc\release\aether-injector.exe"
  $natives[$contract.natives.x86.shim] = "$t\i686-pc-windows-msvc\release\aether_shim_dll.dll"

  # Checked where they were built, before anything is staged: a failed check
  # leaves the staged set as it was.
  Write-Host '== no VC++ runtime imports'
  $dumpbin = $null
  $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
  if (Test-Path $vswhere) {
    $dumpbin = & $vswhere -latest -products '*' -find 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
  }
  if (-not $dumpbin -and ($RequireDumpbin -or @('true', '1') -contains $env:CI)) {
    throw "build-natives: step 'no VC++ runtime imports': dumpbin not found (Visual Studio's MSVC tools)"
  } elseif (-not $dumpbin) {
    Write-Warning "dumpbin not found (Visual Studio's MSVC tools): skipping the VC++ runtime import check"
  } else {
    foreach ($f in $natives.Keys) {
      $global:LASTEXITCODE = 0
      $deps = & $dumpbin /dependents $natives[$f] | Out-String
      if ($LASTEXITCODE) { throw "build-natives: step 'dumpbin $f' failed (exit $LASTEXITCODE)" }
      if ($deps -match '(?i)vcruntime|msvcp') { throw "build-natives: step 'no VC++ runtime imports': $f still imports the VC++ runtime" }
      Write-Host "  ${f}: no VC++ runtime imports"
    }
  }

  # Staged as a set: the four (and whatever else $Out held) are copied into a
  # folder beside $Out, which then takes $Out's place. A copy that fails, or a
  # swap refused because a running program holds a file in $Out, leaves $Out
  # as it was.
  Write-Host "== stage into $Out"
  $next = "$Out.next"
  $prev = "$Out.prev"
  foreach ($d in $next, $prev) {
    if (Test-Path -LiteralPath $d) { Remove-Item -LiteralPath $d -Recurse -Force }
  }
  New-Item -ItemType Directory -Force $next | Out-Null
  try {
    if (Test-Path -LiteralPath $Out) {
      Get-ChildItem -LiteralPath $Out -Force |
        Where-Object { -not $natives.Contains($_.Name) } |
        Copy-Item -Destination $next -Recurse -Force
    }
    foreach ($f in $natives.Keys) { Copy-Item -LiteralPath $natives[$f] -Destination (Join-Path $next $f) }
    if (Test-Path -LiteralPath $Out) { [IO.Directory]::Move($Out, $prev) }
    [IO.Directory]::Move($next, $Out)
  } catch {
    if (-not (Test-Path -LiteralPath $Out) -and (Test-Path -LiteralPath $prev)) { [IO.Directory]::Move($prev, $Out) }
    if (Test-Path -LiteralPath $next) { Remove-Item -LiteralPath $next -Recurse -Force -ErrorAction SilentlyContinue }
    throw "build-natives: step 'stage into $Out' failed, the staged natives are unchanged: $($_.Exception.Message)"
  }
  if (Test-Path -LiteralPath $prev) { Remove-Item -LiteralPath $prev -Recurse -Force }

  if ($Fixtures) {
    # host target, no crt-static: the tests look in target/release.
    # aether-inject's bins include aether-probe.exe, a fixture of its own.
    Invoke-Step 'build host injector and shim' { cargo build --release -p aether-inject -p aether-shim-dll }
    # vproxy's import library must exist before staticimp links against it
    Invoke-Step 'build fixtures' {
      cargo build --release -p aether-fixtures -p aether-fixture-vproxy
    }
    Invoke-Step 'build fixture staticimp' { cargo build --release -p aether-fixture-staticimp }
    # Every file the tests read from target/release (contract/artifacts.json's
    # fixtures), so a build that left one out fails here rather than as a
    # test's skip.
    $release = Join-Path $root 'target\release'
    foreach ($f in @($contract.fixtures)) {
      if (-not (Test-Path -LiteralPath (Join-Path $release $f) -PathType Leaf)) {
        throw "build-natives: step 'build fixtures' did not produce $release\$f"
      }
    }
  }
} finally {
  Pop-Location
}
Write-Host "natives staged in $Out"
