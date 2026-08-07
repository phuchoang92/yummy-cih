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

[[ -n $prefix ]] || { echo "HOME is unset; pass --prefix" >&2; exit 1; }
prefix=$(mkdir -p "$prefix" && cd "$prefix" && pwd)
install_dir="$prefix/opt/cih"
bin_dir="$prefix/bin"
case "$install_dir" in
  /|/bin|/usr|/usr/local|"$HOME") echo "refusing unsafe install directory: $install_dir" >&2; exit 1 ;;
esac

temporary=$(mktemp -d "${TMPDIR:-/tmp}/cih-install.XXXXXXXX")
trap 'rm -rf -- "$temporary"' EXIT

if [[ -z $archive ]]; then
  [[ -n $version ]] || { echo "pass --version or --archive" >&2; exit 2; }
  asset="cih-linux-${version}.tar.gz"
  base="https://github.com/$repository/releases/download/v$version"
  archive="$temporary/$asset"
  checksum="$temporary/cih-linux-${version}.sha256"
  if command -v curl >/dev/null; then
    curl -fL "$base/$asset" -o "$archive"
    curl -fL "$base/cih-linux-${version}.sha256" -o "$checksum"
  elif command -v wget >/dev/null; then
    wget -O "$archive" "$base/$asset"
    wget -O "$checksum" "$base/cih-linux-${version}.sha256"
  else
    echo "curl or wget is required to download CIH" >&2
    exit 1
  fi
else
  archive=$(realpath "$archive")
  if [[ -z $checksum ]]; then
    checksum=${archive%.tar.gz}.sha256
  fi
  checksum=$(realpath "$checksum")
fi

[[ -f $archive && -f $checksum ]] || { echo "archive or checksum is missing" >&2; exit 1; }
expected=$(awk 'NR == 1 && $1 ~ /^[0-9a-fA-F]{64}$/ {print tolower($1)}' "$checksum")
[[ -n $expected ]] || { echo "invalid SHA-256 file: $checksum" >&2; exit 1; }
if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$archive" | awk '{print $1}')
elif command -v shasum >/dev/null; then
  actual=$(shasum -a 256 "$archive" | awk '{print $1}')
else
  echo "sha256sum or shasum is required" >&2
  exit 1
fi
actual=$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')
[[ $actual == "$expected" ]] || { echo "checksum mismatch for $archive" >&2; exit 1; }

mkdir -p "$temporary/staged"
tar -xzf "$archive" --strip-components=1 -C "$temporary/staged"
[[ -x $temporary/staged/bin/cih ]] || { echo "archive does not contain bin/cih" >&2; exit 1; }
CIH_HOME="$temporary/doctor-home" "$temporary/staged/bin/cih" doctor >/dev/null

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
if [[ :${PATH:-}: != *:"$bin_dir":* ]]; then
  echo "Add $bin_dir to PATH, then run: cih doctor"
else
  echo "Run: cih doctor"
fi
