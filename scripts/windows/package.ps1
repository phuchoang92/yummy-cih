[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $Version,
    [string] $TargetDir = "target\x86_64-pc-windows-msvc\release",
    [string] $OutputDir = "dist",
    [Parameter(Mandatory = $true)] [string] $SbomPath,
    [Parameter(Mandatory = $true)] [string] $NoticesPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$target = (Resolve-Path (Join-Path $root $TargetDir)).Path
$exe = Join-Path $target "cih.exe"
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw "cih.exe not found at $exe"
}
foreach ($required in @($SbomPath, $NoticesPath, (Join-Path $root "LICENSE"))) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "required package input is missing: $required"
    }
}
if (-not (Get-Command dumpbin.exe -ErrorAction SilentlyContinue)) {
    throw "dumpbin.exe is required to audit runtime DLL dependencies"
}

$output = Join-Path $root $OutputDir
New-Item -ItemType Directory -Force -Path $output | Out-Null
$stage = Join-Path $output "cih-v$Version-windows-x64"
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item -LiteralPath $exe -Destination $stage
Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination $stage
Copy-Item -LiteralPath $SbomPath -Destination (Join-Path $stage "sbom.cdx.json")
Copy-Item -LiteralPath $NoticesPath -Destination (Join-Path $stage "THIRD_PARTY_NOTICES.txt")
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "install.ps1") -Destination $stage
Copy-Item -LiteralPath (Join-Path $PSScriptRoot "uninstall.ps1") -Destination $stage
$signature = Get-AuthenticodeSignature -LiteralPath $exe
$signatureLabel = if ($signature.Status -eq 'Valid') { "signed" } else { "unsigned ($($signature.Status))" }
Set-Content -LiteralPath (Join-Path $stage "SIGNING_STATUS.txt") -Encoding ascii -Value "$signatureLabel`n"

$systemNames = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
@(
    "advapi32.dll", "bcrypt.dll", "cabinet.dll", "combase.dll", "crypt32.dll",
    "dbghelp.dll", "gdi32.dll", "iphlpapi.dll", "kernel32.dll", "mswsock.dll",
    "ncrypt.dll", "ntdll.dll", "ole32.dll", "oleaut32.dll", "rpcrt4.dll",
    "secur32.dll", "shell32.dll", "shlwapi.dll", "user32.dll", "userenv.dll",
    "ucrtbase.dll", "version.dll", "winhttp.dll", "winmm.dll", "ws2_32.dll"
) | ForEach-Object { [void] $systemNames.Add($_) }

function Get-Dependencies([string] $Binary) {
    $output = & dumpbin.exe /NOLOGO /DEPENDENTS $Binary 2>&1
    if ($LASTEXITCODE -ne 0) { throw "dumpbin failed for $Binary`n$output" }
    @($output | ForEach-Object {
        if ($_ -match '^\s+([A-Za-z0-9_.-]+\.dll)\s*$') { $Matches[1] }
    } | Sort-Object -Unique)
}

function Is-SystemDll([string] $Name) {
    $Name.StartsWith("api-ms-win-", [StringComparison]::OrdinalIgnoreCase) -or
    $Name.StartsWith("ext-ms-win-", [StringComparison]::OrdinalIgnoreCase) -or
    $systemNames.Contains($Name)
}

function Is-ApprovedCompanionDll([string] $Name) {
    $Name -like "libssl-3*.dll" -or
    $Name -like "libcrypto-3*.dll" -or
    $Name -like "vcruntime*.dll" -or
    $Name -like "msvcp*.dll" -or
    $Name -like "concrt*.dll" -or
    $Name -like "vccorlib*.dll" -or
    $Name -ieq "legacy_stdio_definitions.dll"
}

function Resolve-CompanionDll([string] $Name) {
    $roots = [System.Collections.Generic.List[string]]::new()
    $roots.Add($target)
    if ($env:OPENSSL_ROOT_DIR) {
        $roots.Add((Join-Path $env:OPENSSL_ROOT_DIR "bin"))
        $roots.Add($env:OPENSSL_ROOT_DIR)
    }
    foreach ($entry in ($env:PATH -split ';')) {
        if ($entry) { $roots.Add($entry) }
    }
    foreach ($candidateRoot in $roots) {
        $candidate = Join-Path $candidateRoot $Name
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    $where = & where.exe $Name 2>$null
    if ($LASTEXITCODE -eq 0 -and $where) { return @($where)[0] }
    return $null
}

$queue = [System.Collections.Generic.Queue[string]]::new()
$queue.Enqueue((Join-Path $stage "cih.exe"))
$inspected = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
while ($queue.Count -gt 0) {
    $binary = $queue.Dequeue()
    if (-not $inspected.Add($binary)) { continue }
    foreach ($dependency in (Get-Dependencies $binary)) {
        if (Is-SystemDll $dependency) { continue }
        if (-not (Is-ApprovedCompanionDll $dependency)) {
            throw "unapproved runtime DLL '$dependency' required by $binary"
        }
        $destination = Join-Path $stage $dependency
        if (-not (Test-Path -LiteralPath $destination -PathType Leaf)) {
            $source = Resolve-CompanionDll $dependency
            if (-not $source) {
                throw "unresolved or unapproved runtime DLL '$dependency' required by $binary"
            }
            Copy-Item -LiteralPath $source -Destination $destination
        }
        $queue.Enqueue($destination)
    }
}

$openssl = Get-ChildItem -LiteralPath $stage -Filter "libssl-3*.dll"
$crypto = Get-ChildItem -LiteralPath $stage -Filter "libcrypto-3*.dll"
if (-not $openssl -or -not $crypto) {
    throw "OpenSSL 3 companion DLLs were not discovered in the final package"
}

$zip = Join-Path $output "cih-v$Version-windows-x64.zip"
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip -CompressionLevel Optimal
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
$checksum = Join-Path $output "cih-v$Version-windows-x64.sha256"
Set-Content -LiteralPath $checksum -Encoding ascii -NoNewline -Value "$hash  $([IO.Path]::GetFileName($zip))`n"
Write-Host "Packaged $zip"
