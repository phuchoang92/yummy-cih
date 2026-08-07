[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $ZipPath,
    [Parameter(Mandatory = $true)] [string] $ChecksumPath,
    [Parameter(Mandatory = $true)] [string] $WorkDir
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$savedLocalAppData = $env:LOCALAPPDATA
$env:LOCALAPPDATA = ""
$missingLocalAppDataRejected = $false
try {
    & (Join-Path $PSScriptRoot "install.ps1") -ZipPath $ZipPath -ChecksumPath $ChecksumPath
} catch { $missingLocalAppDataRejected = $true }
if (-not $missingLocalAppDataRejected) { throw "installer accepted missing LOCALAPPDATA" }
$env:LOCALAPPDATA = $savedLocalAppData

$env:LOCALAPPDATA = Join-Path $WorkDir "Local App Data"
$env:CIH_HOME = Join-Path $env:LOCALAPPDATA "CIH"
$installDir = Join-Path $env:LOCALAPPDATA "Programs\CIH"
New-Item -ItemType Directory -Force -Path $env:CIH_HOME | Out-Null
Set-Content -LiteralPath (Join-Path $env:CIH_HOME "preserve.txt") -Value "keep"

& (Join-Path $PSScriptRoot "install.ps1") -ZipPath $ZipPath -ChecksumPath $ChecksumPath -InstallDir $installDir
& (Join-Path $PSScriptRoot "install.ps1") -ZipPath $ZipPath -ChecksumPath $ChecksumPath -InstallDir $installDir
$pathEntries = @([Environment]::GetEnvironmentVariable("Path", "User") -split ';' | Where-Object { $_.TrimEnd('\') -ieq $installDir.TrimEnd('\') })
if ($pathEntries.Count -ne 1) { throw "installer PATH update is not idempotent" }

$badZip = Join-Path $WorkDir "corrupt.zip"
Copy-Item -LiteralPath $ZipPath -Destination $badZip
Add-Content -LiteralPath $badZip -Value "corrupt"
$rejected = $false
try {
    & (Join-Path $PSScriptRoot "install.ps1") -ZipPath $badZip -ChecksumPath $ChecksumPath -InstallDir $installDir
} catch { $rejected = $true }
if (-not $rejected) { throw "installer accepted a bad checksum" }

& (Join-Path $PSScriptRoot "uninstall.ps1") -InstallDir $installDir
if (-not (Test-Path -LiteralPath (Join-Path $env:CIH_HOME "preserve.txt"))) { throw "uninstall removed user data" }
& (Join-Path $PSScriptRoot "uninstall.ps1") -InstallDir $installDir -PurgeData
if (Test-Path -LiteralPath $env:CIH_HOME) { throw "-PurgeData did not remove user data" }

Write-Host "Installer lifecycle tests passed"
