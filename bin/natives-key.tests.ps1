# Tests bin/natives-key.ps1's exclusions: every crate it leaves out of the
# key is a crate folder, none of them (nor a fixture crate) is in the
# dependency tree of a native (aether-shim and aether-inject, normal and build
# dependencies, x64 and i686, by cargo tree), and none of their files is among
# the key's inputs. A native that came to depend on an excluded crate would
# otherwise keep an old key, and a release would reuse natives built without
# the change. It also checks that both builders (build-natives.ps1 and
# build-windows) build the four in the one profile, natives-lto.
#
#   powershell -ExecutionPolicy Bypass -File bin\natives-key.tests.ps1
#
# Needs cargo. Exits non-zero, naming the failed cases, when any fails.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$keyScript = Join-Path $PSScriptRoot 'natives-key.ps1'
$failed = New-Object 'System.Collections.Generic.List[string]'
function Check([string]$Name, [bool]$Ok, [string]$Detail = '') {
  if ($Ok) { Write-Host "ok   $Name" } else { Write-Host "FAIL $Name $Detail"; $failed.Add($Name) }
}

# The package names in `cargo tree --prefix none` lines ("name vX.Y.Z (...)")
# that are in $Names.
function Find-Excluded([string[]]$TreeLines, [string[]]$Names) {
  $found = @{}
  foreach ($l in $TreeLines) {
    $n = ($l.Trim() -split ' ')[0]
    if ($n -and ($Names -contains $n)) { $found[$n] = $true }
  }
  @($found.Keys | Sort-Object)
}

# The package name in a crate folder's Cargo.toml.
function Get-PackageName([string]$Dir) {
  $toml = Join-Path $Dir 'Cargo.toml'
  if (-not (Test-Path -LiteralPath $toml)) { return $null }
  foreach ($l in (Get-Content -LiteralPath $toml)) {
    if ($l -match '^\s*name\s*=\s*"([^"]+)"') { return $Matches[1] }
  }
  $null
}

$excluded = @(& powershell -NoProfile -ExecutionPolicy Bypass -File $keyScript -ListExcluded)
Check 'the key script names excluded crates' ($LASTEXITCODE -eq 0 -and $excluded.Count -gt 0) "exit $LASTEXITCODE"

$names = New-Object 'System.Collections.Generic.List[string]'
foreach ($e in $excluded) {
  $n = Get-PackageName (Join-Path $root $e)
  Check "$e is a crate folder" ($null -ne $n)
  if ($n) { $names.Add($n) }
}
foreach ($d in (Get-ChildItem -Directory -Path (Join-Path $root 'crates') -Filter '*fixture*')) {
  $n = Get-PackageName $d.FullName
  if ($n) { $names.Add($n) }
}
Check 'the fixtures are among the excluded' (@($names | Where-Object { $_ -like '*fixture*' }).Count -gt 0)

# The check itself finds an excluded crate in a tree that has one.
$fake = @('aether-shim v0.1.0 (C:\a\crates\aether-shim)', 'aether-conform v0.1.0 (C:\a\crates\aether-conform)', 'windows-sys v0.59.0')
Check 'the check sees an excluded crate in a tree' (((Find-Excluded $fake @('aether-conform', 'aether-filering')) -join ',') -eq 'aether-conform')
Check 'the check passes a tree without one' ((Find-Excluded @('aether-win v0.1.0') @('aether-conform')).Count -eq 0)

foreach ($target in @('x86_64-pc-windows-msvc', 'i686-pc-windows-msvc')) {
  $global:LASTEXITCODE = 0
  $tree = @(& cargo tree --manifest-path (Join-Path $root 'Cargo.toml') -p aether-shim -p aether-inject -e normal,build --prefix none --target $target)
  Check "cargo tree ($target) runs" ($LASTEXITCODE -eq 0 -and $tree.Count -gt 0) "exit $LASTEXITCODE"
  Check "the natives' tree ($target) holds aether-win" ((Find-Excluded $tree @('aether-win')).Count -eq 1)
  $hit = Find-Excluded $tree $names.ToArray()
  Check "no native ($target) depends on an excluded crate" ($hit.Count -eq 0) ($hit -join ', ')
}

$inputs = @(& powershell -NoProfile -ExecutionPolicy Bypass -File $keyScript -ListInputs)
Check 'the key script lists its inputs' ($LASTEXITCODE -eq 0 -and $inputs.Count -gt 0) "exit $LASTEXITCODE"
$leaked = @($inputs | Where-Object { $p = $_; ($p -like 'crates/*fixture*/*') -or @($excluded | Where-Object { $p.StartsWith("$_/") }).Count })
Check 'no excluded file is an input' ($leaked.Count -eq 0) ($leaked -join ', ')
Check 'the shim is an input' (@($inputs | Where-Object { $_ -eq 'crates/aether-shim/Cargo.toml' }).Count -eq 1)
Check 'aether-rpc is an input' (@($inputs | Where-Object { $_ -like 'crates/aether-rpc/*' }).Count -gt 0)

$key = & powershell -NoProfile -ExecutionPolicy Bypass -File $keyScript
Check 'the key is a SHA-256 in hex' ("$key" -match '^[0-9a-f]{64}$') "$key"

# One key, one build: both builders make the four (their --target builds; the
# -Fixtures host pair stays release) in Cargo.toml's natives-lto profile, so a
# release built on either has the same natives.
$cargoToml = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
Check 'Cargo.toml has the natives-lto profile, with fat LTO' ($cargoToml -match '(?ms)^\[profile\.natives-lto\]\s*\r?\n(?:[^\[]*\r?\n)?lto = "fat"')
# The lines of a builder's text that build a native: a --target build naming
# either package, aether-shim or aether-inject (whichever of its bins), other
# than the fixtures' build (build-windows builds aether-probe, a bin of
# aether-inject, and the host pair with the fixtures, in their profile).
function Get-NativeBuilds([string]$Text) {
  # a command continued over lines (bash's trailing backslash) is one line here
  $t = $Text -replace '\\\r?\n', ' '
  @($t -split "`n" | Where-Object { $_ -match 'cargo (xwin )?build' -and $_ -match '--target' -and $_ -match '-p aether-(shim|inject)\b' -and $_ -notmatch '-p aether-fixture' })
}
# Whether a builder builds the natives in three --target builds (the x64 pair,
# the i686 injector, the i686 shim), each in the natives-lto profile.
function Test-NativeBuilds([string]$Text) {
  $lines = @(Get-NativeBuilds $Text)
  $lines.Count -eq 3 -and @($lines | Where-Object { $_ -notmatch '--profile natives-lto|natives_flags' }).Count -eq 0 -and $Text -match 'natives_flags=\(--profile natives-lto\)|--profile natives-lto'
}

# The check itself: a native built outside the profile, even on a line of its
# own or over continued lines, is found.
$lto = 'cargo build --profile natives-lto'
$good = "$lto --target x64 -p aether-inject -p aether-shim`n$lto --target i686 -p aether-inject --bin aether-injector`n$lto --target i686 -p aether-shim`n"
Check 'the builder check passes natives built in the profile' (Test-NativeBuilds $good)
$split = "$lto --target x64 -p aether-shim`ncargo build --release --target x64 -p aether-inject`n$lto --target i686 -p aether-inject --bin aether-injector`n$lto --target i686 -p aether-shim`n"
Check 'the builder check sees an injector built outside the profile' (-not (Test-NativeBuilds $split))
$continued = "$lto --target x64 -p aether-inject -p aether-shim`ncargo build --release --target i686 \`n  -p aether-inject --bin aether-injector`n$lto --target i686 -p aether-shim`n"
Check 'the builder check reads a command continued over lines' (-not (Test-NativeBuilds $continued))

foreach ($builder in @('bin/build-natives.ps1', 'bin/build-windows')) {
  $text = Get-Content -Raw -LiteralPath (Join-Path $root $builder)
  Check "$builder builds the natives in the natives-lto profile" (Test-NativeBuilds $text) ((Get-NativeBuilds $text) -join ' | ')
}
# The fixtures keep their profile on both: build-natives.ps1 -Fixtures builds
# the host injector and shim in release, and build-windows builds them with
# the fixtures and copies them, under the x64 natives' names, from there,
# not from the natives' build.
$bw = (Get-Content -Raw -LiteralPath (Join-Path $root 'bin/build-windows')) -replace '\\\r?\n', ' '
$fixtureBuild = @($bw -split "`n" | Where-Object { $_ -match 'cargo (xwin )?build' -and $_ -match '-p aether-fixtures' -and $_ -match '-p aether-inject\b' -and $_ -match '-p aether-shim\b' -and $_ -notmatch 'natives_flags' })
Check 'bin/build-windows builds the host pair with the fixtures' ($fixtureBuild.Count -eq 1)
Check 'bin/build-windows copies the host pair from the fixtures'' build' ($bw -notmatch '"\$(injector64|shim64)"\) from=')

if ($failed.Count) { Write-Host "$($failed.Count) failed: $($failed -join '; ')"; exit 1 }
Write-Host 'all passed'
exit 0
