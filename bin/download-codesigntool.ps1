# Downloads SSL.com's CodeSignTool (eSigner) into CodeSignTool/ at the repo
# root, where bin/sign.ps1 looks for it, and
# makes its bundled JDK trust the root cs.ssl.com chains to.
#
#   powershell -ExecutionPolicy Bypass -File bin\download-codesigntool.ps1 [-Sandbox]
#
# -Sandbox points the tool at SSL.com's sandbox (cs-try.ssl.com), for testing
# with sandbox credentials.
#
# Copyright (C) 2026 the Cairn authors. Adapted from Vortex's
# download-codesigntool.ps1 (https://github.com/Nexus-Mods/Vortex),
# Copyright (C) the Vortex authors (Nexus Mods), GPL-3.0; see NOTICE.
#
# Runs under Windows PowerShell 5.1. CodeSignTool.zip and CodeSignTool/ are
# git-ignored.
param (
  [switch] $Sandbox
)

Set-StrictMode -Version 'Latest'
$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar makes 5.1's download many times slower
$ProgressPreference = 'SilentlyContinue'

# TLS 1.2 for GitHub: 5.1 may default to older protocols
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$rootDir = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
# Pinned rather than latest: the keytool path below depends on the JDK bundled
# with this release.
$version = 'v1.3.2'
$downloadUrl = "https://github.com/SSLcom/CodeSignTool/releases/download/$version/CodeSignTool-$version-windows.zip"
# The zip's SHA-256 (214,673,784 bytes), from two separate downloads: the
# tool is handed the signing credentials, so a zip that isn't this one is
# never run. A new $version needs its own.
$sha256 = '4afc32e8b7f79bbe1de7e4e7049aaad4e0f754357613b9bbec0e3052f06fd36b'
$downloadedFile = Join-Path $rootDir 'CodeSignTool.zip'
$extractFolder = Join-Path $rootDir 'CodeSignTool'
$rootCert = Join-Path $rootDir 'packaging\certs\SSLcomTLSRSARootCA2022.pem'

Write-Host "Sandbox = $Sandbox"
Write-Host "downloadedFile $downloadedFile"
Write-Host "extractFolder $extractFolder"

# A fresh folder each time: keytool refuses an alias that is already there
if (Test-Path $extractFolder) {
  Remove-Item -Path $extractFolder -Recurse -Force
}

# Download (unless it already is)
if (!(Test-Path $downloadedFile -PathType Leaf)) {
  Invoke-WebRequest -UseBasicParsing -OutFile $downloadedFile $downloadUrl
}

# Check it, a cached zip too: a wrong one is deleted, so the next run fetches
# it again
$actual = (Get-FileHash -Algorithm SHA256 -Path $downloadedFile).Hash.ToLowerInvariant()
if ($actual -ne $sha256) {
  Remove-Item -Path $downloadedFile -Force
  throw "CodeSignTool.zip's SHA-256 is $actual, not the pinned $sha256 (deleted it)"
}

Expand-Archive -Path $downloadedFile -DestinationPath $extractFolder -Force

# Some releases nest everything in one folder: lift its contents up a level
# so CodeSignTool.bat is at the top
$dirs = @(Get-ChildItem $extractFolder -Directory)
if ($dirs.Count -eq 1 -and !(Test-Path (Join-Path $extractFolder 'CodeSignTool.bat'))) {
  $nested = $dirs[0].FullName
  Write-Host "nestedFolderPath $nested"
  Get-ChildItem -Path $nested -Force | Move-Item -Destination $extractFolder
  Remove-Item -Path $nested -Force
}

if (!(Test-Path (Join-Path $extractFolder 'CodeSignTool.bat') -PathType Leaf)) {
  throw "CodeSignTool.bat is not in $extractFolder"
}

# The bundled JDK 11.0.2 doesn't trust SSL.com TLS RSA Root CA 2022, which
# cs.ssl.com has chained to since 2026-09-22, so every sign call fails the TLS
# handshake. Add the root to that JDK's cacerts.
$keytool = Join-Path $extractFolder 'jdk-11.0.2\bin\keytool.exe'
& $keytool -importcert -noprompt -cacerts -storepass changeit -alias sslcom-tls-rsa-root-2022 -file $rootCert
if ($LASTEXITCODE -ne 0) {
  throw "Could not add $rootCert to CodeSignTool's JDK"
}

# Point the tool at the sandbox (only while testing)
if ($Sandbox) {
  $properties = Join-Path $extractFolder 'conf\code_sign_tool.properties'
  $null = New-Item -Path $properties -ItemType File -Force
  Add-Content -Path $properties -Value 'CLIENT_ID=qOUeZCCzSqgA93acB3LYq6lBNjgZdiOxQc-KayC3UMw'
  Add-Content -Path $properties -Value 'OAUTH2_ENDPOINT=https://oauth-sandbox.ssl.com/oauth2/token'
  Add-Content -Path $properties -Value 'CSC_API_ENDPOINT=https://cs-try.ssl.com'
  Add-Content -Path $properties -Value 'TSA_URL=http://ts.ssl.com'
}
