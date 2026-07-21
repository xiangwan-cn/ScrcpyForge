#!/bin/sh
set -eu
version=4.0
expected=84924bd564a1eb6089c872c7521f968058977f91f5ff02514a8c74aff3210f3a
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target="$root/third_party/scrcpy-server-v${version}.jar"
mkdir -p "$root/third_party"
curl --fail --location --retry 3 "https://github.com/Genymobile/scrcpy/releases/download/v${version}/scrcpy-server-v${version}" --output "$target"
actual=$(sha256sum "$target" | cut -d ' ' -f 1)
if [ "$actual" != "$expected" ]; then
    echo "scrcpy-server checksum mismatch" >&2
    exit 1
fi
echo "$target"
