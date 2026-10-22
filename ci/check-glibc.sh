#!/usr/bin/env bash
set -euo pipefail
binary=$(realpath "$1")
versions=$(objdump -T "$binary" | awk '/\*UND\*/ {print}' | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' | sort -Vu)
if [[ -z "$versions" ]]; then
    echo 'No imported GLIBC versions found' >&2
    exit 1
fi
maximum=$(tail -n 1 <<< "$versions")
echo "GLIBC maximum: $maximum ($binary)"
[[ $(printf '%s\n' "$maximum" 2.36 | sort -V | tail -n 1) == 2.36 ]]
