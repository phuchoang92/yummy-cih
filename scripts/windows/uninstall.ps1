[CmdletBinding()]
param(
    [string] $InstallDir = "$env:LOCALAPPDATA\Programs\CIH",
    [switch] $PurgeData
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if (-not $env:LOCALAPPDATA) { throw "LOCALAPPDATA is not set" }

function Assert-SafeManagedDirectory([string] $Path, [string] $Label) {
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $root = [IO.Path]::GetPathRoot($full).TrimEnd('\')
    $protected = @($root, $env:USERPROFILE, $env:LOCALAPPDATA) |
        Where-Object { $_ } |
        ForEach-Object { [IO.Path]::GetFullPath($_).TrimEnd('\') }
    if (-not $full -or $protected -contains $full) {
        throw "$Label resolves to protected directory '$full'"
    }
    return $full
}

$installFull = Assert-SafeManagedDirectory $InstallDir "InstallDir"
$running = @(Get-Process -Name cih -ErrorAction SilentlyContinue | Where-Object {
    try { $_.Path -and [IO.Path]::GetFullPath($_.Path).StartsWith($installFull + '\', [StringComparison]::OrdinalIgnoreCase) }
    catch { $false }
})
if ($running.Count -gt 0) {
    throw "CIH is running from $InstallDir; stop it before uninstalling"
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$entries = @($userPath -split ';' | Where-Object {
    $_ -and $_.TrimEnd('\') -ine $installFull
})
[Environment]::SetEnvironmentVariable("Path", ($entries -join ';'), "User")

if (Test-Path -LiteralPath $installFull) {
    Remove-Item -LiteralPath $installFull -Recurse -Force
}

$dataDir = if ($env:CIH_HOME) { $env:CIH_HOME } else { Join-Path $env:LOCALAPPDATA "CIH" }
if ($PurgeData -and (Test-Path -LiteralPath $dataDir)) {
    $safeDataDir = Assert-SafeManagedDirectory $dataDir "CIH data directory"
    Remove-Item -LiteralPath $safeDataDir -Recurse -Force
    Write-Host "Removed CIH and user data at $safeDataDir"
} else {
    Write-Host "Removed CIH. User data preserved at $dataDir"
}
