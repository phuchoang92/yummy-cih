#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: uninstall.sh [--prefix DIR] [--purge-data]"
}

prefix=${HOME:+$HOME/.local}
purge_data=0
while (($#)); do
  case "$1" in
    --prefix) prefix=${2:?}; shift 2 ;;
    --purge-data) purge_data=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n $prefix ]] || { echo "HOME is unset; pass --prefix" >&2; exit 1; }
prefix=$(cd "$prefix" 2>/dev/null && pwd -P || printf '%s' "$prefix")
install_dir="$prefix/opt/cih"
link="$prefix/bin/cih"
case "$install_dir" in
  /|/bin|/usr|/usr/local|"${HOME:-/__unset__}") echo "refusing unsafe install directory: $install_dir" >&2; exit 1 ;;
esac

if [[ -L $link ]]; then
  target=$(readlink "$link")
  if [[ $target == "$install_dir/bin/cih" ]]; then
    rm -f -- "$link"
  fi
fi
if [[ -d $install_dir ]]; then
  rm -rf -- "$install_dir"
fi

if ((purge_data)); then
  data_dir=${CIH_HOME:-${HOME:+$HOME/.cih}}
  [[ -n $data_dir ]] || { echo "cannot determine CIH data directory" >&2; exit 1; }
  if [[ -d $data_dir ]]; then
    data_dir=$(cd "$data_dir" && pwd -P)
  elif [[ $data_dir != /* ]]; then
    data_dir="$(pwd -P)/$data_dir"
  fi
  case "$data_dir" in
    /|"${HOME:-/__unset__}") echo "refusing unsafe data directory: $data_dir" >&2; exit 1 ;;
  esac
  [[ -d $data_dir ]] && rm -rf -- "$data_dir"
  echo "Removed CIH installation and data at $data_dir"
else
  echo "Removed CIH installation; data was preserved"
fi
