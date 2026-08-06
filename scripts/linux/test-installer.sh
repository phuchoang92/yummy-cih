#!/usr/bin/env bash
set -euo pipefail

archive=${1:?usage: test-installer.sh ARCHIVE CHECKSUM}
checksum=${2:?usage: test-installer.sh ARCHIVE CHECKSUM}
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/cih-installer-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
export HOME="$work/home"
export CIH_HOME="$work/data"
mkdir -p "$HOME" "$CIH_HOME"
prefix="$HOME/.local"

"$root/scripts/linux/install.sh" --archive "$archive" --checksum "$checksum" --prefix "$prefix"
"$prefix/bin/cih" doctor >/dev/null
"$root/scripts/linux/install.sh" --archive "$archive" --checksum "$checksum" --prefix "$prefix"
[[ -L $prefix/bin/cih && -x $prefix/opt/cih/bin/cih ]]

bad_checksum="$work/bad.sha256"
printf '%064d  %s\n' 0 "$(basename "$archive")" >"$bad_checksum"
if "$root/scripts/linux/install.sh" --archive "$archive" --checksum "$bad_checksum" --prefix "$prefix"; then
  echo "installer accepted an invalid checksum" >&2
  exit 1
fi

touch "$CIH_HOME/preserved"
"$prefix/opt/cih/uninstall.sh" --prefix "$prefix"
[[ -f $CIH_HOME/preserved && ! -e $prefix/bin/cih && ! -e $prefix/opt/cih ]]

"$root/scripts/linux/install.sh" --archive "$archive" --checksum "$checksum" --prefix "$prefix"
"$prefix/opt/cih/uninstall.sh" --prefix "$prefix" --purge-data
[[ ! -e $CIH_HOME ]]
echo "Linux installer lifecycle passed"
