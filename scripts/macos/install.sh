#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  install.sh --version VERSION [--repository OWNER/REPO] [--prefix DIR]
  install.sh --archive FILE [--checksum FILE] [--prefix DIR]
EOF
}

version=
repository=phuchoang92/yummy-cih
archive=
checksum=
prefix=${HOME:+$HOME/.local}

while (($#)); do
  case "$1" in
    --version) version=${2:?}; shift 2 ;;
    --repository) repository=${2:?}; shift 2 ;;
    --archive) archive=${2:?}; shift 2 ;;
    --checksum) checksum=${2:?}; shift 2 ;;
    --prefix) prefix=${2:?}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Darwin ]] || { echo "this installer supports macOS only" >&2; exit 1; }
[[ -n $prefix ]] || { echo "HOME is unset; pass --prefix" >&2; exit 1; }

detect_arch() {
  local machine
  machine=$(uname -m)
  if [[ $machine == x86_64 && $(sysctl -in sysctl.proc_translated 2>/dev/null || true) == 1 ]]; then
    machine=arm64
  fi
  case "$machine" in
    arm64) printf 'arm64\n' ;;
    x86_64) printf 'x86_64\n' ;;
    *) echo "unsupported macOS architecture: $machine" >&2; return 1 ;;
  esac
}

version_at_least() {
  awk -v actual="$1" -v required="$2" 'BEGIN {
    split(actual, a, "."); split(required, r, ".");
    for (i = 1; i <= 3; i++) {
      av = a[i] + 0; rv = r[i] + 0;
      if (av > rv) exit 0;
      if (av < rv) exit 1;
    }
    exit 0;
  }'
}

macos_version=$(sw_vers -productVersion)
version_at_least "$macos_version" 13.3 || {
  echo "CIH requires macOS 13.3 or newer; found $macos_version" >&2
  exit 1
}
arch=$(detect_arch)

prefix=$(mkdir -p "$prefix" && cd "$prefix" && pwd -P)
install_dir="$prefix/opt/cih"
bin_dir="$prefix/bin"
case "$install_dir" in
  /|/bin|/usr|/usr/local|"${HOME:-/__unset__}") echo "refusing unsafe install directory: $install_dir" >&2; exit 1 ;;
esac

temporary=$(mktemp -d "${TMPDIR:-/tmp}/cih-macos-install.XXXXXXXX")
trap 'rm -rf -- "$temporary"' EXIT

absolute_file() {
  local directory basename
  directory=$(cd "$(dirname "$1")" && pwd -P)
  basename=$(basename "$1")
  printf '%s/%s\n' "$directory" "$basename"
}

if [[ -z $archive ]]; then
  [[ -n $version ]] || { echo "pass --version or --archive" >&2; exit 2; }
  asset="cih-macos-$arch-${version}.tar.gz"
  base="https://github.com/$repository/releases/download/v$version"
  archive="$temporary/$asset"
  checksum="$temporary/cih-macos-$arch-${version}.sha256"
  curl -fL "$base/$asset" -o "$archive"
  curl -fL "$base/cih-macos-$arch-${version}.sha256" -o "$checksum"
else
  archive=$(absolute_file "$archive")
  if [[ -z $checksum ]]; then
    checksum=${archive%.tar.gz}.sha256
  fi
  checksum=$(absolute_file "$checksum")
fi

[[ -f $archive && -f $checksum ]] || { echo "archive or checksum is missing" >&2; exit 1; }
expected=$(awk 'NR == 1 && $1 ~ /^[0-9a-fA-F]{64}$/ {print tolower($1)}' "$checksum")
[[ -n $expected ]] || { echo "invalid SHA-256 file: $checksum" >&2; exit 1; }
actual=$(shasum -a 256 "$archive" | awk '{print $1}')
actual=$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')
[[ $actual == "$expected" ]] || { echo "checksum mismatch for $archive" >&2; exit 1; }

mkdir -p "$temporary/staged"
COPYFILE_DISABLE=1 tar -xzf "$archive" --strip-components=1 -C "$temporary/staged"
binary="$temporary/staged/bin/cih"
[[ -x $binary ]] || { echo "archive does not contain bin/cih" >&2; exit 1; }
binary_archs=$(lipo -archs "$binary")
[[ $binary_archs == "$arch" ]] || {
  echo "package architecture '$binary_archs' does not match this Mac ($arch)" >&2
  exit 1
}

if xattr -p com.apple.quarantine "$binary" >/dev/null 2>&1; then
  echo "the downloaded package is quarantined and is not notarized" >&2
  echo "review the release, then remove quarantine from the archive and rerun:" >&2
  echo "  xattr -d com.apple.quarantine '$archive'" >&2
  exit 1
fi

CIH_HOME="$temporary/doctor-home" "$binary" doctor >/dev/null

mkdir -p "$(dirname "$install_dir")" "$bin_dir"
backup="$temporary/previous"
if [[ -e $install_dir ]]; then
  mv "$install_dir" "$backup"
fi
if ! mv "$temporary/staged" "$install_dir"; then
  [[ -e $backup ]] && mv "$backup" "$install_dir"
  exit 1
fi
ln -sfn "$install_dir/bin/cih" "$bin_dir/cih"

echo "CIH installed at $install_dir"
echo "This build is ad-hoc signed and not notarized by Apple."
if [[ :${PATH:-}: != *:"$bin_dir":* ]]; then
  echo "Add $bin_dir to PATH, then run: cih doctor"
else
  echo "Run: cih doctor"
fi
