#!/usr/bin/env bash
# Verify the tier-1 linux-musl release archives in $1 (default dist) and unpack
# the binaries to bin/{amd64,arm64}/ferryman for Dockerfile.release. The
# version comes from the artifact name; both archives must agree. Writes it to
# bin/VERSION and prints it (stdout is ONLY the version; all else goes to stderr).
set -euo pipefail
dist=${1:-dist}
out=${2:-bin}
shopt -s nullglob
amd=("$dist"/ferryman-v*-x86_64-unknown-linux-musl.tar.gz)
[ "${#amd[@]}" -eq 1 ] || { echo "expected exactly one x86_64 musl archive in $dist" >&2; exit 1; }
base=${amd[0]##*/}
version=${base#ferryman-v}
version=${version%-x86_64-unknown-linux-musl.tar.gz}
mkdir -p "$out"
for pair in amd64:x86_64 arm64:aarch64; do
  arch=${pair%%:*}
  name="ferryman-v${version}-${pair##*:}-unknown-linux-musl"
  (cd "$dist" && sha256sum -c "$name.tar.gz.sha256" >&2)  # stdout is the version only
  mkdir -p "$out/$arch"
  tar -xzf "$dist/$name.tar.gz" -C "$out/$arch" --strip-components=1 "$name/ferryman"
done
printf '%s\n' "$version" > "$out/VERSION"
ls -l "$out"/*/ferryman >&2
echo "$version"
