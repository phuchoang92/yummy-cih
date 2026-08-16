# macOS portable CIH

The macOS release provides one `cih` command for Apple Silicon and Intel Macs
running macOS 13.3 or newer. It bundles LadybugDB and OpenSSL and does not
require Rust, Homebrew, Docker, FalkorDB, Postgres, Java, Node.js, or downloaded
models.

The two native packages are `cih-macos-arm64-<version>.tar.gz` and
`cih-macos-x86_64-<version>.tar.gz`. The installer selects the correct one,
including when an Apple Silicon shell is running through Rosetta.

## Install without root

Download `install-macos.sh` from the matching GitHub Release and run:

```bash
bash install-macos.sh --version 0.2.0
export PATH="$HOME/.local/bin:$PATH"
cih doctor
```

The default installation is under `$HOME/.local/opt/cih`, with a symlink at
`$HOME/.local/bin/cih`. Pass `--prefix /another/location` to change both paths.
The installer verifies the release SHA-256 checksum, rejects a package for the
wrong CPU architecture, and runs the staged binary's doctor checks before
replacing an existing installation.

To install an already downloaded package:

```bash
bash install-macos.sh \
  --archive ./cih-macos-arm64-0.2.0.tar.gz \
  --checksum ./cih-macos-arm64-0.2.0.sha256
```

The archive can also run without installation:

```bash
tar xzf cih-macos-arm64-0.2.0.tar.gz
./cih-macos-arm64-0.2.0/bin/cih doctor
```

Keep the `bin` and `lib` directories together. The executable uses a relative
runtime search path to load its packaged LadybugDB and OpenSSL libraries.

## Signing and Gatekeeper

The macOS package is ad-hoc signed to protect Mach-O integrity after its bundled
library paths are rewritten. It is not signed with an Apple Developer ID and is
not notarized. `SIGNING_STATUS.txt` inside the archive records that status.

Downloads performed by `install-macos.sh` use `curl` and normally do not carry a
quarantine attribute. A package downloaded through a browser may be quarantined
and blocked by Gatekeeper. After verifying the release URL and SHA-256 checksum,
remove quarantine only from the downloaded archive and rerun the installer:

```bash
xattr -d com.apple.quarantine ./cih-macos-arm64-0.2.0.tar.gz
bash install-macos.sh \
  --archive ./cih-macos-arm64-0.2.0.tar.gz \
  --checksum ./cih-macos-arm64-0.2.0.sha256
```

Do not disable Gatekeeper globally. Developer ID signing and notarization require
an Apple Developer Program account and are intentionally absent from this release.

## Use

```bash
cih index /path/to/repository
cih serve /path/to/repository --bind 127.0.0.1:8080 --open
```

MCP is served at `/mcp`, the browser at `/graph`, and readiness endpoints at
`/health` and `/ready`. State defaults to `$HOME/.cih`; set `CIH_HOME` to use a
different writable directory.

The portable profile is local and offline. Docker-oriented `start` and semantic
`embed` are not included; BM25 is used for text retrieval. Direct engine commands
such as `cih analyze`, `cih discover`, `cih wiki`, and `cih group` remain
available.

## Uninstall

Run the installed uninstaller or the standalone release asset:

```bash
~/.local/opt/cih/uninstall.sh
```

This removes the command while preserving `$HOME/.cih`. Delete CIH data only
when explicitly requested:

```bash
~/.local/opt/cih/uninstall.sh --purge-data
```

## Release contents and qualification

Each architecture includes the tarball, SHA-256 checksum, CycloneDX SBOM,
third-party notices, installer/uninstaller, signing status, qualification record,
and GitHub build provenance. CI audits every Mach-O dependency, requires bundled
LadybugDB/OpenSSL resolution, verifies ad-hoc signatures, and exercises doctor,
index, serve, MCP, reinstall, checksum rejection, architecture rejection,
uninstall preservation, and purge on native GitHub-hosted runners.

Merging a new workspace version to `master` creates its `v<version>` tag and
dispatches the Linux, Windows, and macOS release builds automatically. If that
version tag already belongs to an earlier commit, bump
`[workspace.package].version` before merging another release.
