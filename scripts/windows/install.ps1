[CmdletBinding(DefaultParameterSetName = "Download")]
param(
    [Parameter(Mandatory = $true, ParameterSetName = "Download")] [string] $Version,
    [Parameter(ParameterSetName = "Download")] [string] $Repository = "phuchoang92/yummy-cih",
    [Parameter(Mandatory = $true, ParameterSetName = "Local")] [string] $ZipPath,
    [Parameter(ParameterSetName = "Local")] [string] $ChecksumPath,
    [string] $InstallDir = "$env:LOCALAPPDATA\Programs\CIH"
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

$temporary = Join-Path ([IO.Path]::GetTempPath()) ("cih-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    if ($PSCmdlet.ParameterSetName -eq "Download") {
        $asset = "cih-v$Version-windows-x64.zip"
        $base = "https://github.com/$Repository/releases/download/v$Version"
        $ZipPath = Join-Path $temporary $asset
        $ChecksumPath = Join-Path $temporary "$asset.sha256"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $ZipPath
        Invoke-WebRequest -UseBasicParsing -Uri "$base/cih-v$Version-windows-x64.sha256" -OutFile $ChecksumPath
    } else {
        $ZipPath = (Resolve-Path -LiteralPath $ZipPath).Path
        if (-not $ChecksumPath) {
            $ChecksumPath = [IO.Path]::ChangeExtension($ZipPath, ".sha256")
        }
        $ChecksumPath = (Resolve-Path -LiteralPath $ChecksumPath).Path
    }

    $checksumLine = (Get-Content -LiteralPath $ChecksumPath -Raw).Trim()
    if ($checksumLine -notmatch '^([0-9a-fA-F]{64})\s+') {
        throw "invalid SHA-256 checksum file: $ChecksumPath"
    }
    $expected = $Matches[1].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { throw "checksum mismatch for $ZipPath" }

    $staged = Join-Path $temporary "staged"
    Expand-Archive -LiteralPath $ZipPath -DestinationPath $staged
    $stagedExe = Join-Path $staged "cih.exe"
    if (-not (Test-Path -LiteralPath $stagedExe -PathType Leaf)) {
        throw "archive does not contain cih.exe at its root"
    }

    & $stagedExe doctor
    if ($LASTEXITCODE -ne 0) { throw "staged cih doctor failed" }

    $installFull = Assert-SafeManagedDirectory $InstallDir "InstallDir"
    $running = @(Get-Process -Name cih -ErrorAction SilentlyContinue | Where-Object {
        try { $_.Path -and [IO.Path]::GetFullPath($_.Path).StartsWith($installFull + '\', [StringComparison]::OrdinalIgnoreCase) }
        catch { $false }
    })
    if ($running.Count -gt 0) {
        throw "CIH is running from $InstallDir; stop it before installing or upgrading"
    }

    $parent = Split-Path -Parent $installFull
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $backup = "$installFull.previous"
    if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Recurse -Force }
    if (Test-Path -LiteralPath $installFull) { Move-Item -LiteralPath $installFull -Destination $backup }
    try {
        Move-Item -LiteralPath $staged -Destination $installFull
    } catch {
        if (Test-Path -LiteralPath $backup) { Move-Item -LiteralPath $backup -Destination $installFull }
        throw
    }
    if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Recurse -Force }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if (-not ($entries | Where-Object { $_.TrimEnd('\') -ieq $installFull })) {
        $newPath = (@($entries) + $installFull) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    }
    Write-Host "CIH installed at $installFull"
    Write-Host "Open a new terminal and run: cih doctor"
} finally {
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
}
