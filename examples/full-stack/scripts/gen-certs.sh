#!/bin/sh
# Generates a throwaway CA + server cert for the full-stack demo's TLS
# termination. Idempotent: does nothing if the CA already exists, so
# repeated `docker compose up` runs don't rotate certs under a live proxy.
#
# Usage: gen-certs.sh [output-dir]   (default: certs, relative to cwd)
#
# Runs inside the `certgen` alpine:3.20 compose service, with
# `output-dir` set to the container's bind-mounted /certs.

set -eu

OUT_DIR="${1:-certs}"
mkdir -p "$OUT_DIR"

if [ -f "$OUT_DIR/ca.pem" ]; then
    echo "[gen-certs] $OUT_DIR/ca.pem already exists, skipping"
    exit 0
fi

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

# The ferryman container (FROM scratch, runs as root) and the host both
# need to read these; make them world-readable rather than relying on
# matching uids across containers.
chmod 0644 "$OUT_DIR"/*.pem

echo "[gen-certs] done"
