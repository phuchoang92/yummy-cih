[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $Exe,
    [Parameter(Mandatory = $true)] [string] $Fixture,
    [Parameter(Mandatory = $true)] [string] $WorkDir,
    [int] $Port = 18080
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Exe = (Resolve-Path -LiteralPath $Exe).Path
$Fixture = (Resolve-Path -LiteralPath $Fixture).Path
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$env:CIH_HOME = Join-Path $WorkDir "CIH Home 数据"
$repo = Join-Path $WorkDir ("fixture repo 日本語 " + (("x" * 96) -join ""))
if (Test-Path -LiteralPath $repo) { Remove-Item -LiteralPath $repo -Recurse -Force }
Copy-Item -LiteralPath $Fixture -Destination $repo -Recurse

function Start-CihProcess {
    param(
        [Parameter(Mandatory = $true)] [string[]] $Arguments,
        [string] $WorkingDirectory
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Exe
    $startInfo.UseShellExecute = $false
    foreach ($argument in $Arguments) {
        [void] $startInfo.ArgumentList.Add($argument)
    }
    if ($WorkingDirectory) {
        $startInfo.WorkingDirectory = $WorkingDirectory
    }
    return [Diagnostics.Process]::Start($startInfo)
}

$normalHome = $env:CIH_HOME
$readOnlyHome = Join-Path $WorkDir "read only home"
New-Item -ItemType Directory -Force -Path $readOnlyHome | Out-Null
$originalAcl = Get-Acl -LiteralPath $readOnlyHome
$deniedAcl = Get-Acl -LiteralPath $readOnlyHome
$identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$denyWrite = [Security.AccessControl.FileSystemAccessRule]::new(
    $identity,
    [Security.AccessControl.FileSystemRights]::Write,
    [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit,
    [Security.AccessControl.PropagationFlags]::None,
    [Security.AccessControl.AccessControlType]::Deny
)
[void] $deniedAcl.AddAccessRule($denyWrite)
Set-Acl -LiteralPath $readOnlyHome -AclObject $deniedAcl
try {
    $env:CIH_HOME = $readOnlyHome
    $doctorError = Join-Path $WorkDir "read-only-doctor.stderr"
    $doctorJson = ((& $Exe doctor --json 2>$doctorError) -join "`n")
    if ($LASTEXITCODE -eq 0) { throw "doctor accepted a read-only CIH_HOME" }
    $doctorReport = $doctorJson | ConvertFrom-Json
    if ($doctorReport.home.ok) { throw "doctor did not identify its read-only home check" }
} finally {
    Set-Acl -LiteralPath $readOnlyHome -AclObject $originalAcl
    $env:CIH_HOME = $normalHome
}

& $Exe index $repo --force
if ($LASTEXITCODE -ne 0) { throw "cih index failed" }
foreach ($path in @(
    (Join-Path $env:CIH_HOME "registry.json"),
    (Join-Path $repo ".cih\repository-identity.json"),
    (Join-Path $repo ".cih\wiki")
)) {
    if (-not (Test-Path -LiteralPath $path)) { throw "missing index output: $path" }
}

# A second valid registry entry makes implicit selection from an unrelated
# directory an actionable error rather than an arbitrary choice.
$secondRepo = Join-Path $WorkDir "second fixture"
Copy-Item -LiteralPath $Fixture -Destination $secondRepo -Recurse
& $Exe index $secondRepo --force --no-wiki
if ($LASTEXITCODE -ne 0) { throw "second repository index failed" }
$ambiguous = Start-CihProcess -Arguments @("serve") -WorkingDirectory $WorkDir
if (-not $ambiguous.WaitForExit(5000)) {
    Stop-Process -Id $ambiguous.Id
    throw "cih serve without a repo did not reject an ambiguous registry"
}
if ($ambiguous.ExitCode -eq 0) { throw "cih serve accepted an ambiguous registry" }

$server = Start-CihProcess -Arguments @("serve", $repo, "--bind", "127.0.0.1:$Port")
try {
    $ready = $false
    for ($attempt = 0; $attempt -lt 120; $attempt++) {
        if ($server.HasExited) { throw "cih serve exited with $($server.ExitCode)" }
        try {
            $response = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/ready"
            if ($response.StatusCode -eq 200) { $ready = $true; break }
        } catch {}
        Start-Sleep -Milliseconds 250
    }
    if (-not $ready) { throw "cih serve did not become ready" }

    $conflict = Start-CihProcess -Arguments @("serve", $repo, "--bind", "127.0.0.1:$Port")
    if (-not $conflict.WaitForExit(5000)) {
        Stop-Process -Id $conflict.Id
        throw "second server did not reject a conflicting port"
    }
    if ($conflict.ExitCode -eq 0) { throw "second server accepted a conflicting port" }

    foreach ($route in @("health", "ready", "graph")) {
        $response = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/$route"
        if ($response.StatusCode -ne 200) { throw "/$route returned $($response.StatusCode)" }
    }

    $headers = @{ Accept = "application/json, text/event-stream"; "Content-Type" = "application/json" }
    $initialize = @{
        jsonrpc = "2.0"; id = 1; method = "initialize"; params = @{
            protocolVersion = "2025-06-18"; capabilities = @{}; clientInfo = @{ name = "windows-smoke"; version = "1" }
        }
    } | ConvertTo-Json -Depth 8 -Compress
    $response = Invoke-WebRequest -UseBasicParsing -Method Post -Uri "http://127.0.0.1:$Port/mcp" -Headers $headers -Body $initialize
    $session = @($response.Headers.GetValues("Mcp-Session-Id")) | Select-Object -First 1
    if (-not $session) { throw "MCP initialize returned no session id" }
    $headers["Mcp-Session-Id"] = [string] $session
    $notification = @{ jsonrpc = "2.0"; method = "notifications/initialized"; params = @{} } | ConvertTo-Json -Compress
    Invoke-WebRequest -UseBasicParsing -Method Post -Uri "http://127.0.0.1:$Port/mcp" -Headers $headers -Body $notification | Out-Null
    $list = @{ jsonrpc = "2.0"; id = 2; method = "tools/list"; params = @{} } | ConvertTo-Json -Compress
    $listed = Invoke-WebRequest -UseBasicParsing -Method Post -Uri "http://127.0.0.1:$Port/mcp" -Headers $headers -Body $list
    if ($listed.Content -notmatch 'search_code' -or $listed.Content -notmatch 'query') { throw "tools/list missing query tools" }
    foreach ($call in @(
        @{ id = 3; name = "search_code"; arguments = @{ query = "order service"; limit = 5 } },
        @{ id = 4; name = "query"; arguments = @{ q = "order service"; limit = 5 } },
        @{ id = 5; name = "communities"; arguments = @{ limit = 10 } }
    )) {
        $body = @{ jsonrpc = "2.0"; id = $call.id; method = "tools/call"; params = @{ name = $call.name; arguments = $call.arguments } } | ConvertTo-Json -Depth 8 -Compress
        $called = Invoke-WebRequest -UseBasicParsing -Method Post -Uri "http://127.0.0.1:$Port/mcp" -Headers $headers -Body $body
        if ($called.Content -match '"isError"\s*:\s*true') { throw "$($call.name) returned an MCP error" }
        if ($call.name -in @("search_code", "query") -and $called.Content -notmatch 'bm25') {
            throw "$($call.name) did not report the portable BM25 source"
        }
    }

    # Re-index while the server retains its current Ladybug reader, then prove
    # the newly published version is immediately queryable.
    & $Exe index $repo --force --no-wiki
    if ($LASTEXITCODE -ne 0) { throw "concurrent re-index failed" }
    $response = Invoke-WebRequest -UseBasicParsing -Uri "http://127.0.0.1:$Port/ready"
    if ($response.StatusCode -ne 200) { throw "server not ready after re-index" }
} finally {
    if (-not $server.HasExited) {
        Stop-Process -Id $server.Id
        $server.WaitForExit()
    }
}

Write-Host "Windows portable smoke test passed"
