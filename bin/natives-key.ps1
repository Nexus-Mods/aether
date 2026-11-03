# Prints the natives key: what the four Windows natives are built from, as
# one lowercase hex SHA-256. A release whose key equals an earlier release's
# reuses that release's signed natives (release.yml).
#
#   powershell -ExecutionPolicy Bypass -File bin\natives-key.ps1
#
# The inputs are the files git tracks among Cargo.toml, Cargo.lock,
# rust-toolchain.toml, vendor/, crates/ and bin/build-natives.ps1, read from
# the working tree, except the fixtures' crates (crates/*fixture*) and the
# test kit (crates/vfs-testkit): a change there changes no native. They are
# sorted by path (ordinal), and each is hashed as its UTF-8 path, a NUL, its
# length in decimal, a NUL, then its bytes. So any change to an input's
# content, a new or removed input or a rename gives another key. Whatever
# else the build reads belongs among the inputs too: Cargo configuration in a
# .cargo/ folder would have to be added here.
#
# This is Cairn's formula (its bin/natives-key.ps1) over Aether's inputs.
# .gitattributes fixes every text file's line endings (eol=lf), so a Windows
# and a Linux checkout give the same key. Runs under Windows PowerShell 5.1
# and PowerShell 7.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot

# git's paths are UTF-8: read them as such, whatever the console's code page
$oldEncoding = [Console]::OutputEncoding
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
try {
  $global:LASTEXITCODE = 0
  $listed = & git -C $root ls-files -z -- Cargo.toml Cargo.lock rust-toolchain.toml vendor crates bin/build-natives.ps1 ':(exclude,glob)crates/*fixture*/**' ':(exclude)crates/vfs-testkit'
  if ($LASTEXITCODE) { throw "natives-key: git ls-files failed (exit $LASTEXITCODE)" }
} finally {
  [Console]::OutputEncoding = $oldEncoding
}

$paths = New-Object 'System.Collections.Generic.List[string]'
foreach ($chunk in @($listed)) {
  foreach ($p in ($chunk -split "`0")) { if ($p) { $paths.Add($p) } }
}
if ($paths.Count -eq 0) { throw "natives-key: git lists none of the inputs" }
$paths.Sort([StringComparer]::Ordinal)

$utf8 = New-Object System.Text.UTF8Encoding $false
$nul = [byte[]]@(0)
$sha = [Security.Cryptography.SHA256]::Create()
try {
  foreach ($p in $paths) {
    $bytes = [IO.File]::ReadAllBytes((Join-Path $root $p))
    foreach ($part in @($utf8.GetBytes($p), $nul, $utf8.GetBytes($bytes.Length.ToString([Globalization.CultureInfo]::InvariantCulture)), $nul, $bytes)) {
      [void]$sha.TransformBlock($part, 0, $part.Length, $null, 0)
    }
  }
  [void]$sha.TransformFinalBlock((New-Object byte[] 0), 0, 0)
  ($sha.Hash | ForEach-Object { $_.ToString('x2') }) -join ''
} finally {
  $sha.Dispose()
}
