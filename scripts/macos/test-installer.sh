#!/usr/bin/env bash
set -euo pipefail

archive=${1:?usage: test-installer.sh ARCHIVE CHECKSUM}
checksum=${2:?usage: test-installer.sh ARCHIVE CHECKSUM}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/cih-macos-installer-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
export HOME="$work/home"
export CIH_HOME="$work/data"
mkdir -p "$HOME" "$CIH_HOME"
prefix="$HOME/.local"

"$root/scripts/macos/install.sh" --archive "$archive" --checksum "$checksum" --prefix "$prefix"
"$prefix/bin/cih" doctor >/dev/null
"$root/scripts/macos/install.sh" --archive "$archive" --checksum "$checksum" --prefix "$prefix"
[[ -L $prefix/bin/cih && -x $prefix/opt/cih/bin/cih ]]

bad_checksum="$work/bad.sha256"
printf '%064d  %s\n' 0 "$(basename "$archive")" >"$bad_checksum"
if "$root/scripts/macos/install.sh" --archive "$archive" --checksum "$bad_checksum" --prefix "$prefix"; then
  echo "installer accepted an invalid checksum" >&2
  exit 1
fi

# Prove that architecture validation happens before executing the staged CLI.
current=$(uname -m)
if [[ $current == arm64 ]]; then
  opposite=x86_64
else
  opposite=arm64e
fi
mkdir -p "$work/wrong/cih-macos-wrong/bin"
lipo /usr/bin/true -thin "$opposite" -output "$work/wrong/cih-macos-wrong/bin/cih"
chmod +x "$work/wrong/cih-macos-wrong/bin/cih"
wrong_archive="$work/cih-macos-wrong.tar.gz"
COPYFILE_DISABLE=1 tar -C "$work/wrong" -czf "$wrong_archive" cih-macos-wrong
wrong_checksum="$work/cih-macos-wrong.sha256"
(cd "$work" && shasum -a 256 "$(basename "$wrong_archive")" >"$(basename "$wrong_checksum")")
if "$root/scripts/macos/install.sh" --archive "$wrong_archive" --checksum "$wrong_checksum" --prefix "$prefix"; then
  echo "installer accepted a package for the wrong architecture" >&2
  exit 1
fi

touch "$CIH_HOME/preserved"
"$prefix/opt/cih/uninstall.sh" --prefix "$prefix"
[[ -f $CIH_HOME/preserved && ! -e $prefix/bin/cih && ! -e $prefix/opt/cih ]]

"$root/scripts/macos/install.sh" --archive "$archive" --checksum "$checksum" --prefix "$prefix"
"$prefix/opt/cih/uninstall.sh" --prefix "$prefix" --purge-data
[[ ! -e $CIH_HOME ]]
echo "macOS installer lifecycle passed"
