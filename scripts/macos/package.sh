#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: package.sh --version VERSION --arch arm64|x86_64 --lbug-dir DIR \
  --openssl-lib-dir DIR --sbom FILE --notices FILE [--target-dir DIR] \
  [--output-dir DIR]
EOF
}

version=
arch=
lbug_dir=
openssl_lib_dir=
sbom=
notices=
target_dir=target/release
output_dir=dist

while (($#)); do
  case "$1" in
    --version) version=${2:?}; shift 2 ;;
    --arch) arch=${2:?}; shift 2 ;;
    --lbug-dir) lbug_dir=${2:?}; shift 2 ;;
    --openssl-lib-dir) openssl_lib_dir=${2:?}; shift 2 ;;
    --sbom) sbom=${2:?}; shift 2 ;;
    --notices) notices=${2:?}; shift 2 ;;
    --target-dir) target_dir=${2:?}; shift 2 ;;
    --output-dir) output_dir=${2:?}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for value in version arch lbug_dir openssl_lib_dir sbom notices; do
  if [[ -z ${!value} ]]; then
    echo "missing required option: $value" >&2
    usage >&2
    exit 2
  fi
done
[[ $arch == arm64 || $arch == x86_64 ]] || { echo "unsupported architecture: $arch" >&2; exit 2; }
[[ $version =~ ^[0-9A-Za-z][0-9A-Za-z.-]*$ ]] || { echo "invalid package version: $version" >&2; exit 2; }

for command in codesign install_name_tool lipo otool shasum tar; do
  command -v "$command" >/dev/null || {
    echo "$command is required to build the macOS portable package" >&2
    exit 1
  }
done

absolute_file() {
  local directory basename
  directory=$(cd "$(dirname "$1")" && pwd -P)
  basename=$(basename "$1")
  printf '%s/%s\n' "$directory" "$basename"
}

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
if [[ $target_dir == /* ]]; then
  target_dir=$(cd "$target_dir" && pwd -P)
else
  target_dir=$(cd "$root/$target_dir" && pwd -P)
fi
output_dir=$(cd "$root" && mkdir -p "$output_dir" && cd "$output_dir" && pwd -P)
lbug_dir=$(cd "$lbug_dir" && pwd -P)
openssl_lib_dir=$(cd "$openssl_lib_dir" && pwd -P)
sbom=$(absolute_file "$sbom")
notices=$(absolute_file "$notices")

binary="$target_dir/cih"
[[ -x $binary ]] || { echo "cih executable not found at $binary" >&2; exit 1; }
[[ -f $root/LICENSE ]] || { echo "LICENSE is missing" >&2; exit 1; }
[[ -f $sbom && -f $notices ]] || { echo "SBOM or notices file is missing" >&2; exit 1; }

binary_archs=$(lipo -archs "$binary")
[[ $binary_archs == "$arch" ]] || {
  echo "cih executable must contain only $arch: $binary_archs" >&2
  exit 1
}

stage_name="cih-macos-$arch-$version"
stage="$output_dir/$stage_name"
case "$stage" in
  "$output_dir"/cih-macos-arm64-*|"$output_dir"/cih-macos-x86_64-*) ;;
  *) echo "refusing unsafe staging path: $stage" >&2; exit 1 ;;
esac
rm -rf -- "$stage"
mkdir -p "$stage/bin" "$stage/lib"

install -m 0755 "$binary" "$stage/bin/cih"
install -m 0644 "$root/LICENSE" "$stage/LICENSE"
install -m 0644 "$sbom" "$stage/sbom-macos-$arch.cdx.json"
install -m 0644 "$notices" "$stage/THIRD_PARTY_NOTICES-macos-$arch.txt"
install -m 0755 "$root/scripts/macos/install.sh" "$stage/install.sh"
install -m 0755 "$root/scripts/macos/uninstall.sh" "$stage/uninstall.sh"

lbug_source=$(find "$lbug_dir" -maxdepth 1 -type f -name 'liblbug.0.18.2.dylib' -print | head -n 1)
[[ -n $lbug_source ]] || { echo "liblbug.0.18.2.dylib was not found in $lbug_dir" >&2; exit 1; }
install -m 0755 "$lbug_source" "$stage/lib/liblbug.0.18.2.dylib"
ln -s liblbug.0.18.2.dylib "$stage/lib/liblbug.0.dylib"
ln -s liblbug.0.dylib "$stage/lib/liblbug.dylib"

for library in libssl.3.dylib libcrypto.3.dylib; do
  source_path="$openssl_lib_dir/$library"
  [[ -f $source_path ]] || { echo "$library was not found in $openssl_lib_dir" >&2; exit 1; }
  install -m 0755 "$source_path" "$stage/lib/$library"
done

lbug="$stage/lib/liblbug.0.18.2.dylib"
ssl="$stage/lib/libssl.3.dylib"
crypto="$stage/lib/libcrypto.3.dylib"

install_name_tool -id '@rpath/liblbug.0.dylib' "$lbug"
install_name_tool -id '@rpath/libssl.3.dylib' "$ssl"
install_name_tool -id '@rpath/libcrypto.3.dylib' "$crypto"

rewrite_dependency() {
  local subject=$1 suffix=$2 replacement=$3 dependency
  while IFS= read -r dependency; do
    [[ $dependency == *"/$suffix" ]] || continue
    install_name_tool -change "$dependency" "$replacement" "$subject"
  done < <(otool -L "$subject" | tail -n +2 | awk '{print $1}')
}

for subject in "$stage/bin/cih" "$lbug" "$ssl" "$crypto"; do
  rewrite_dependency "$subject" liblbug.0.dylib '@rpath/liblbug.0.dylib'
  rewrite_dependency "$subject" libssl.3.dylib '@rpath/libssl.3.dylib'
  rewrite_dependency "$subject" libcrypto.3.dylib '@rpath/libcrypto.3.dylib'
done

remove_rpaths() {
  local subject=$1 rpath
  while IFS= read -r rpath; do
    install_name_tool -delete_rpath "$rpath" "$subject"
  done < <(otool -l "$subject" | awk '/cmd LC_RPATH/{getline; getline; print $2}')
}

remove_rpaths "$stage/bin/cih"
install_name_tool -add_rpath '@executable_path/../lib' "$stage/bin/cih"
for library in "$lbug" "$ssl" "$crypto"; do
  remove_rpaths "$library"
  install_name_tool -add_rpath '@loader_path' "$library"
done

audit_macho() {
  local subject=$1 dependency architectures minimum
  while IFS= read -r dependency; do
    case "$dependency" in
      /usr/lib/*|/System/Library/*|@rpath/liblbug.0.dylib|@rpath/libssl.3.dylib|@rpath/libcrypto.3.dylib) ;;
      *) echo "unapproved runtime dependency for $subject: $dependency" >&2; return 1 ;;
    esac
  done < <(otool -L "$subject" | tail -n +2 | awk '{print $1}')
  architectures=$(lipo -archs "$subject")
  [[ $architectures == "$arch" ]] || {
    echo "$subject must contain only $arch: $architectures" >&2
    return 1
  }
  minimum=$(otool -l "$subject" | awk '$1 == "minos" {print $2; exit}')
  [[ -n $minimum ]] || { echo "cannot determine minimum macOS for $subject" >&2; return 1; }
  awk -v actual="$minimum" -v maximum=13.3 'BEGIN {
    split(actual, a, "."); split(maximum, m, ".");
    for (i = 1; i <= 3; i++) {
      av = a[i] + 0; mv = m[i] + 0;
      if (av < mv) exit 0;
      if (av > mv) exit 1;
    }
    exit 0;
  }' || {
    echo "$subject requires macOS $minimum, newer than the 13.3 contract" >&2
    return 1
  }

  local expected_rpath rpaths
  if [[ $subject == "$stage/bin/cih" ]]; then
    expected_rpath='@executable_path/../lib'
  else
    expected_rpath='@loader_path'
  fi
  rpaths=$(otool -l "$subject" | awk '/cmd LC_RPATH/{getline; getline; print $2}')
  [[ $rpaths == "$expected_rpath" ]] || {
    echo "$subject has unexpected runtime search paths: $rpaths" >&2
    return 1
  }
}

for subject in "$stage/bin/cih" "$lbug" "$ssl" "$crypto"; do
  audit_macho "$subject"
done

# install_name_tool invalidates existing signatures. Sign nested code first and
# the executable last; an ad-hoc signature preserves Mach-O integrity but is not
# a Developer ID signature and cannot be notarized.
for library in "$lbug" "$ssl" "$crypto"; do
  codesign --force --sign - "$library"
  codesign --verify --strict --verbose=2 "$library"
done
codesign --force --sign - "$stage/bin/cih"
codesign --verify --strict --verbose=2 "$stage/bin/cih"

cat >"$stage/SIGNING_STATUS.txt" <<'EOF'
ad-hoc signed; not Developer ID signed; not notarized
EOF

cat >"$stage/BUILD_INFO.txt" <<EOF
CIH version: $version
Target: $arch-apple-darwin
Minimum macOS: 13.3
LadybugDB: 0.18.2
OpenSSL: 3.5.7
Source commit: ${SOURCE_COMMIT:-unknown}
Signing: ad-hoc; not notarized
EOF

archive="$output_dir/$stage_name.tar.gz"
checksum="$output_dir/$stage_name.sha256"
rm -f -- "$archive" "$checksum"
COPYFILE_DISABLE=1 tar -C "$output_dir" -czf "$archive" "$stage_name"
(cd "$output_dir" && shasum -a 256 "$stage_name.tar.gz" >"$stage_name.sha256")

echo "Packaged $archive"
