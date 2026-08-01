# Windows portable CIH

The Windows release provides one `cih.exe` command for Windows 10/11 x64. It
indexes repositories, runs the local Ladybug graph database, serves MCP and the
graph browser, generates documentation, and provides graph and BM25 search. Rust,
Docker, FalkorDB, Postgres, Java, and Node.js are not required.

The portable ZIP is a self-contained directory, not a literal single-file build:
keep `cih.exe` beside the OpenSSL and any packaged MSVC runtime DLLs.

## Install

Download `install.ps1` from the matching GitHub Release and run it in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\install.ps1 -Version 0.1.0
```

The installer downloads the ZIP and checksum, verifies SHA-256, runs the staged
binary's doctor checks, installs into `%LOCALAPPDATA%\Programs\CIH`, and adds that
exact directory to the user PATH without administrator rights. A local, already
downloaded package can be installed with:

```powershell
.\install.ps1 `
  -ZipPath .\cih-v0.1.0-windows-x64.zip `
  -ChecksumPath .\cih-v0.1.0-windows-x64.sha256
```

Installation and upgrades refuse to replace a running installed `cih.exe`. Stop
the foreground server and retry; the installer never kills it automatically.

## Use

```powershell
# Diagnose the install, registry, embedded database, native DLLs, and port 8080.
cih doctor

# Index the current directory or an explicit repository. This performs analyze,
# package discovery, and graph-mode wiki generation entirely locally.
cih index
cih index C:\src\my-service --force --json
cih index C:\src\my-service --no-wiki

# Serve MCP and the graph browser in the foreground. Ctrl+C stops it.
cih serve C:\src\my-service --open
cih serve C:\src\my-service --bind 127.0.0.1:9090
```

MCP is available at `/mcp`, the browser at `/graph`, and readiness endpoints at
`/health` and `/ready`. `cih serve` chooses the primary repository from an
explicit argument, the indexed repository containing the current directory, or
the sole valid registry entry, in that order. If several repositories remain,
pass one explicitly. Other registered repositories remain selectable through
tool `repo` arguments.

Engine commands that work with the local profile remain available directly, such
as `cih analyze`, `cih discover`, `cih wiki`, and `cih group`. Docker-oriented
`start` and semantic `embed` are intentionally absent. Search reports BM25 as its
source; semantic/vector search and bundled models are not part of this release.

## Data directories and compatibility

`CIH_HOME` overrides every CIH data path. Without it, Windows uses
`%LOCALAPPDATA%\CIH`; graphs live in its `graphs` subdirectory and registry,
groups, contracts, and configuration remain under the home directory.

Legacy `%USERPROFILE%\.cih` data is not moved automatically. `cih doctor` reports
it and gives two safe choices: copy it to `%LOCALAPPDATA%\CIH`, or set `CIH_HOME`
to the legacy path.

## Uninstall

Run the uninstaller from the installed directory or release assets:

```powershell
.\uninstall.ps1
```

It removes the installation and the exact user PATH entry while preserving
`%LOCALAPPDATA%\CIH`. Data is deleted only when explicitly requested:

```powershell
.\uninstall.ps1 -PurgeData
```

Like upgrades, uninstall refuses to run while the installed CIH process is active.

## Release qualification

The Windows workflow builds with the pinned Rust toolchain and locked dependencies,
tests the engine/server feature matrix and Ladybug contract suite, runs an offline
Unicode/spaced-path index-and-serve smoke test, re-indexes while the server holds an
older graph version, audits every non-system DLL recursively, and tests install,
reinstall, checksum rejection, PATH idempotence, uninstall preservation, and purge.

Every tag release includes the ZIP, SHA-256 checksum, CycloneDX SBOM, third-party
notices, installer scripts, and build provenance. `SIGNING_STATUS.txt` records
whether optional Authenticode signing was applied. Before publishing broadly, run
the same ZIP on clean Windows 10 and Windows 11 x64 VMs with no developer tools or
runtime prerequisites installed; hosted GitHub Windows runners are not substitutes
for that final desktop-OS qualification.
