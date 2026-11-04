# Checks files' Authenticode signatures, as release.yml does for the natives
# it signs or reuses. Prints each file's status and signer,
# and writes one line to the output per problem: a file missing, a status
# other than Valid, or (with -Signer) a signer whose subject isn't -Signer.
# No output means every file passed. It never throws for a bad file: the
# caller decides what a problem means.
#
#   $bad = @(& .\bin\check-signatures.ps1 -Path a.exe, b.dll [-Signer 'CN=...'])
#
# -Signer defaults to $env:EXPECTED_SIGNER (release.yml sets it from the
# repository variable of that name). One rule: -Signer must be exactly SignerCertificate.Subject (CN first, Windows' key
# names), compared case-sensitively ignoring only the spaces after commas
# (', +' in Get-NormalName).
# Without -Signer, the signer isn't compared, and a warning says so.
#
# Runs under Windows PowerShell 5.1 and PowerShell 7.
param(
  [Parameter(Mandatory = $true)][string[]]$Path,
  [string]$Signer = $env:EXPECTED_SIGNER
)
$ErrorActionPreference = 'Stop'

function Get-NormalName([string]$Name) { $Name -replace ', +', ',' }

if (-not $Signer) {
  Write-Host "::warning::EXPECTED_SIGNER (the repository variable) is not set: signatures are checked for Valid, not for who signed them"
}

foreach ($p in $Path) {
  if (-not (Test-Path -LiteralPath $p -PathType Leaf)) {
    "$p (missing)"
    continue
  }
  $s = Get-AuthenticodeSignature -LiteralPath $p
  $subject = ''
  if ($s.SignerCertificate) { $subject = $s.SignerCertificate.Subject }
  Write-Host "$($s.Status) $p $subject"
  if ($s.Status -ne 'Valid') {
    "$p ($($s.Status))"
  } elseif ($Signer -and ((Get-NormalName $subject) -cne (Get-NormalName $Signer))) {
    "$p (signed by '$subject', not '$Signer')"
  }
}
