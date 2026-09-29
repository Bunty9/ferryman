#!/bin/sh
# Generates a throwaway CA + server cert for the full-stack demo's TLS
# termination. Idempotent: does nothing if a complete set of certs already
# exists, so repeated `docker compose up` runs don't rotate certs under a
# live proxy.
#
# Usage: gen-certs.sh [output-dir]   (default: certs, relative to cwd)
#
# Runs inside the `certgen` alpine:3.20 compose service, with
# `output-dir` set to the container's bind-mounted /certs.

set -eu

OUT_DIR="${1:-certs}"
mkdir -p "$OUT_DIR"

if [ -f "$OUT_DIR/ca.pem" ] && [ -f "$OUT_DIR/server.pem" ] && [ -f "$OUT_DIR/server-key.pem" ]; then
    echo "[gen-certs] $OUT_DIR already has ca.pem/server.pem/server-key.pem, skipping generation"
    # Enforce perms every run, even when skipping generation: an older
    # checkout (from before this chmod existed, or a partial/interrupted
    # run) can leave a world-readable ca-key.pem sitting there indefinitely
    # otherwise, since generation itself never runs again.
    if [ -f "$OUT_DIR/ca-key.pem" ]; then chmod 0600 "$OUT_DIR/ca-key.pem"; fi
    chmod 0644 "$OUT_DIR/server.pem" "$OUT_DIR/server-key.pem" "$OUT_DIR/ca.pem"
    exit 0
fi
echo "[gen-certs] $OUT_DIR is missing one or more of ca.pem/server.pem/server-key.pem, (re)generating"

command -v openssl >/dev/null 2>&1 || apk add --no-cache openssl >/dev/null

echo "[gen-certs] generating CA + server cert in $OUT_DIR"

openssl genrsa -out "$OUT_DIR/ca-key.pem" 2048 >/dev/null 2>&1
openssl req -x509 -new -nodes -key "$OUT_DIR/ca-key.pem" -sha256 -days 3650 \
    -subj "/CN=ferryman-demo-ca" -out "$OUT_DIR/ca.pem" >/dev/null 2>&1

openssl genrsa -out "$OUT_DIR/server-key.pem" 2048 >/dev/null 2>&1
openssl req -new -key "$OUT_DIR/server-key.pem" -subj "/CN=ferryman" \
    -out "$OUT_DIR/server.csr" >/dev/null 2>&1

cat >"$OUT_DIR/server-ext.cnf" <<EOF
subjectAltName = DNS:localhost,DNS:ferryman,IP:127.0.0.1
extendedKeyUsage = serverAuth
EOF

openssl x509 -req -in "$OUT_DIR/server.csr" \
    -CA "$OUT_DIR/ca.pem" -CAkey "$OUT_DIR/ca-key.pem" -CAcreateserial \
    -out "$OUT_DIR/server.pem" -days 3650 -sha256 \
    -extfile "$OUT_DIR/server-ext.cnf" >/dev/null 2>&1

rm -f "$OUT_DIR/server.csr" "$OUT_DIR/ca.srl" "$OUT_DIR/server-ext.cnf"

# The ferryman container (FROM scratch, runs as root) and the host both need
# to read the server cert/key ferryman actually serves with, so those two
# stay world-readable rather than relying on matching uids across
# containers. The CA *key* is different: nothing needs it at runtime (it
# only ever signs the server cert, once, right here) and it's the one file
# in this directory that lets someone mint their own trusted certs for this
# demo CA, so lock it down instead of leaving it world-readable too.
chmod 0644 "$OUT_DIR/server.pem" "$OUT_DIR/server-key.pem" "$OUT_DIR/ca.pem"
chmod 0600 "$OUT_DIR/ca-key.pem"

# Note for cleanup: openssl in the certgen container runs as root, so these
# files end up root-owned on a Linux host (harmless for docker compose
# itself, which reads them as root inside containers, but it means a plain
# `rm -rf certs/*` from your own shell may need `sudo`). `docker compose
# down -v` doesn't touch this bind-mounted directory either way.
echo "[gen-certs] done"
