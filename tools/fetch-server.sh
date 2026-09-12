#!/bin/sh
set -eu
version=4.0
expected=84924bd564a1eb6089c872c7521f968058977f91f5ff02514a8c74aff3210f3a
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target="$root/third_party/scrcpy-server-v${version}.jar"
temporary="$target.tmp.$$"
trap 'rm -f "$temporary"' EXIT HUP INT TERM
mkdir -p "$root/third_party"
curl --fail --location --retry 3 "https://github.com/Genymobile/scrcpy/releases/download/v${version}/scrcpy-server-v${version}" --output "$temporary"
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary" | cut -d ' ' -f 1)
else
    actual=$(shasum -a 256 "$temporary" | cut -d ' ' -f 1)
fi
if [ "$actual" != "$expected" ]; then
    echo "scrcpy-server checksum mismatch" >&2
    exit 1
fi
mv -f "$temporary" "$target"
trap - EXIT HUP INT TERM
echo "$target"
