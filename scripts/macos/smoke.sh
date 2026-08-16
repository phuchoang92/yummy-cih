#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
export CIH_SMOKE_PLATFORM=macOS
exec "$root/scripts/linux/smoke.sh" "$@"
