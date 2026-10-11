#!/usr/bin/env bash
# Self-test for extract-release-bins.sh: builds fake archives in the release
# layout and asserts stdout is exactly the version (callers capture it).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
ver=1.2.3-rc.1
mkdir "$tmp/dist"
for t in x86_64 aarch64; do
  name="ferryman-v$ver-$t-unknown-linux-musl"
  mkdir -p "$tmp/src/$name"
  echo "fake $t" > "$tmp/src/$name/ferryman"
  tar -czf "$tmp/dist/$name.tar.gz" -C "$tmp/src" "$name"
  (cd "$tmp/dist" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
done
got=$("$here/extract-release-bins.sh" "$tmp/dist" "$tmp/bin" 2>/dev/null)
[ "$got" = "$ver" ] || { echo "stdout was '$got', want '$ver'" >&2; exit 1; }
[ "$(cat "$tmp/bin/amd64/ferryman")" = "fake x86_64" ] && [ "$(cat "$tmp/bin/arm64/ferryman")" = "fake aarch64" ]
echo "ok: stdout == $ver"
