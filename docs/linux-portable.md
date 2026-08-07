# Linux portable CIH

The Linux x64 release provides one `cih` command for glibc-based distributions
and containers. It indexes repositories, stores graphs in embedded LadybugDB,
serves MCP and the graph browser, generates documentation, and provides BM25
search without Rust, Java, Docker, FalkorDB, Postgres, or downloaded models.

The release supports glibc 2.28 or newer. Alpine and other musl-only systems are
not supported natively; use the portable OCI image there instead.

## Install without root

Download `install.sh` from the matching GitHub Release and run:

```bash
bash install.sh --version 0.1.0
export PATH="$HOME/.local/bin:$PATH"
cih doctor
```

The default installation is under `$HOME/.local/opt/cih`, with a symlink at
`$HOME/.local/bin/cih`. Pass `--prefix /another/location` to change both paths.
The installer verifies the release SHA-256 checksum and runs the staged binary's
doctor checks before replacing an existing installation.

To install an already downloaded package:

```bash
bash install.sh \
  --archive ./cih-linux-0.1.0.tar.gz \
  --checksum ./cih-linux-0.1.0.sha256
```

The archive can also run without installation:

```bash
tar xzf cih-linux-0.1.0.tar.gz
./cih-linux-0.1.0/bin/cih doctor
```

Keep the `bin` and `lib` directories together. The executable uses a relative
runtime search path to load its packaged LadybugDB and OpenSSL libraries.

## Native use

```bash
cih index /path/to/repository
cih serve /path/to/repository --bind 127.0.0.1:8080 --open
```

MCP is served at `/mcp`, the graph browser at `/graph`, and readiness endpoints
at `/health` and `/ready`. State defaults to `$HOME/.cih`; set `CIH_HOME` to use
another writable directory.

The portable profile is deliberately offline and local. `start` and `embed` are
not included, semantic/vector search is disabled, and BM25 is used for text
retrieval. Direct engine commands such as `cih analyze`, `cih discover`, `cih
wiki`, and `cih group` remain available.

## Run the portable OCI image

The image contains exactly the executable and native libraries qualified in the
Linux tarball. Use a host-owned data directory and run with the host UID/GID so
the process can write both the mounted repository's `.cih` directory and its
persistent embedded graphs:

```bash
repo=/absolute/path/to/repository
mkdir -p "$PWD/.cih-portable-data"

docker run --rm \
  --user "$(id -u):$(id -g)" \
  -e HOME=/tmp/cih-user \
  -e CIH_HOME=/data \
  -v "$PWD/.cih-portable-data:/data" \
  -v "$repo:/repo" \
  phuchoang29/yummy-cih:portable-latest \
  index /repo

docker run --rm -p 8080:8080 \
  --user "$(id -u):$(id -g)" \
  -e HOME=/tmp/cih-user \
  -e CIH_HOME=/data \
  -v "$PWD/.cih-portable-data:/data" \
  -v "$repo:/repo" \
  phuchoang29/yummy-cih:portable-latest \
  serve /repo --bind 0.0.0.0:8080
```

Use `portable-v<version>` instead of `portable-latest` for a pinned deployment.
The repository must be mounted at the same absolute container path during
`index` and `serve`, because its registry entry and source locators preserve that
path.

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

Each Linux release includes the tarball, SHA-256 checksum, CycloneDX
SBOM, third-party notices, installer/uninstaller, qualification record, and
GitHub build provenance. CI audits every ELF dependency and rejects a package
that requires a glibc symbol newer than 2.28 or resolves LadybugDB/OpenSSL from
outside the bundle. The same package is exercised on Rocky Linux 8, Ubuntu
20.04, Debian 12, and in the published OCI image.

Merging a new workspace version to `master` creates its `v<version>` tag and
dispatches the Linux and Windows release builds automatically. If that version
tag already belongs to an earlier commit, bump `[workspace.package].version`
before merging another release.
