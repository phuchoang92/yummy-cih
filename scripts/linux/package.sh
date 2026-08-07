#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: package.sh --version VERSION --lbug-dir DIR --openssl-lib-dir DIR \
  --sbom FILE --notices FILE [--target-dir DIR] [--output-dir DIR]
EOF
}

version=
lbug_dir=
openssl_lib_dir=
sbom=
notices=
target_dir=target/release
output_dir=dist

while (($#)); do
  case "$1" in
    --version) version=${2:?}; shift 2 ;;
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

for value in version lbug_dir openssl_lib_dir sbom notices; do
  if [[ -z ${!value} ]]; then
    echo "missing required option: $value" >&2
    usage >&2
    exit 2
  fi
done

for command in patchelf readelf ldd sha256sum tar; do
  command -v "$command" >/dev/null || {
    echo "$command is required to build the Linux portable package" >&2
    exit 1
  }
done

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
target_dir=$(cd "$root" && realpath "$target_dir")
output_dir=$(cd "$root" && mkdir -p "$output_dir" && realpath "$output_dir")
lbug_dir=$(realpath "$lbug_dir")
openssl_lib_dir=$(realpath "$openssl_lib_dir")
sbom=$(realpath "$sbom")
notices=$(realpath "$notices")

binary="$target_dir/cih"
[[ -x $binary ]] || { echo "cih executable not found at $binary" >&2; exit 1; }
[[ -f $root/LICENSE ]] || { echo "LICENSE is missing" >&2; exit 1; }

stage_name="cih-linux-${version}"
stage="$output_dir/$stage_name"
case "$stage" in
  "$output_dir"/cih-linux-*) ;;
  *) echo "refusing unsafe staging path: $stage" >&2; exit 1 ;;
esac
rm -rf -- "$stage"
mkdir -p "$stage/bin" "$stage/lib"

install -m 0755 "$binary" "$stage/bin/cih"
install -m 0644 "$root/LICENSE" "$stage/LICENSE"
install -m 0644 "$sbom" "$stage/sbom-linux-x64.cdx.json"
install -m 0644 "$notices" "$stage/THIRD_PARTY_NOTICES-linux-x64.txt"
install -m 0755 "$root/scripts/linux/install.sh" "$stage/install.sh"
install -m 0755 "$root/scripts/linux/uninstall.sh" "$stage/uninstall.sh"

lbug_source=$(find "$lbug_dir" -maxdepth 1 -type f -name 'liblbug.so.*' | sort -V | tail -n 1)
[[ -n $lbug_source ]] || { echo "versioned liblbug.so was not found in $lbug_dir" >&2; exit 1; }
install -m 0755 "$lbug_source" "$stage/lib/liblbug.so.0.18.2"
ln -s liblbug.so.0.18.2 "$stage/lib/liblbug.so.0"
ln -s liblbug.so.0 "$stage/lib/liblbug.so"

for library in libssl.so.3 libcrypto.so.3; do
  source_path=$(find "$openssl_lib_dir" -maxdepth 1 \( -type f -o -type l \) -name "$library" | head -n 1)
  [[ -n $source_path ]] || { echo "$library was not found in $openssl_lib_dir" >&2; exit 1; }
  install -m 0755 "$source_path" "$stage/lib/$library"
done

# Literal ELF loader token; it must not expand in the packaging shell.
# shellcheck disable=SC2016
patchelf --set-rpath '$ORIGIN/../lib' "$stage/bin/cih"
for library in "$stage/lib/liblbug.so.0.18.2" "$stage/lib/libssl.so.3" "$stage/lib/libcrypto.so.3"; do
  # shellcheck disable=SC2016
  patchelf --set-rpath '$ORIGIN' "$library"
done

approved_system='^(linux-vdso\.so\.1|ld-linux-x86-64\.so\.2|libc\.so\.6|libdl\.so\.2|libgcc_s\.so\.1|libm\.so\.6|libpthread\.so\.0|librt\.so\.1|libutil\.so\.1)$'
audit_binary() {
  local subject=$1 dependency resolved
  while IFS= read -r line; do
    [[ $line == *"not found"* ]] && { echo "unresolved runtime dependency for $subject: $line" >&2; return 1; }
    dependency=$(awk '{print $1}' <<<"$line")
    [[ $dependency == /* ]] && dependency=$(basename "$dependency")
    [[ $dependency == linux-vdso.so.1 || $dependency == ld-linux-x86-64.so.2 || $dependency == *.so* ]] || continue
    if [[ $dependency =~ $approved_system ]]; then
      continue
    fi
    case "$dependency" in
      liblbug.so.0|libssl.so.3|libcrypto.so.3) ;;
      *) echo "unapproved runtime dependency for $subject: $dependency" >&2; return 1 ;;
    esac
    resolved=$(awk '/=>/ {print $3}' <<<"$line")
    [[ $resolved == "$stage/lib/"* ]] || {
      echo "$dependency for $subject resolved outside the bundle: $resolved" >&2
      return 1
    }
  done < <(LD_LIBRARY_PATH="$stage/lib" ldd "$subject")
}

audit_binary "$stage/bin/cih"
audit_binary "$stage/lib/liblbug.so.0.18.2"
audit_binary "$stage/lib/libssl.so.3"
audit_binary "$stage/lib/libcrypto.so.3"

max_glibc=0
while IFS= read -r subject; do
  subject_max=$(readelf --version-info "$subject" 2>/dev/null |
    sed -n 's/.*Name: GLIBC_\([0-9][0-9.]*\).*/\1/p' |
    sort -V | tail -n 1)
  if [[ -n $subject_max && $(printf '%s\n%s\n' "$max_glibc" "$subject_max" | sort -V | tail -n 1) == "$subject_max" ]]; then
    max_glibc=$subject_max
  fi
done < <(find "$stage/bin" "$stage/lib" -type f -print)
if [[ $(printf '%s\n%s\n' 2.28 "$max_glibc" | sort -V | tail -n 1) != 2.28 ]]; then
  echo "portable bundle requires GLIBC_$max_glibc, newer than the GLIBC_2.28 contract" >&2
  exit 1
fi

cat >"$stage/BUILD_INFO.txt" <<EOF
CIH version: $version
Target: x86_64-unknown-linux-gnu
glibc baseline: 2.28
LadybugDB: 0.18.2
OpenSSL: 3.5.7
Source commit: ${SOURCE_COMMIT:-unknown}
Maximum required GLIBC symbol: $max_glibc
EOF

archive="$output_dir/$stage_name.tar.gz"
checksum="$output_dir/$stage_name.sha256"
rm -f -- "$archive" "$checksum"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='UTC 1970-01-01' \
  -C "$output_dir" -czf "$archive" "$stage_name"
(cd "$output_dir" && sha256sum "$stage_name.tar.gz" >"$stage_name.sha256")

echo "Packaged $archive"
