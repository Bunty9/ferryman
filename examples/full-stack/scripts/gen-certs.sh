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

# docker-compose.yml bind-mounts ./certs/server.pem and
# ./certs/server-key.pem as single files into the ferryman container. If
# ferryman starts before this script has ever run (e.g. `docker compose up
# --no-deps ferryman` on a fresh clone), Docker creates the missing host
# path as a *directory*, since it has no way to know the container side is
# meant to be a file. openssl's `-out` would then fail on that path with a
# cryptic, redirected-to-/dev/null error, so catch it here with a clear
# message instead.
for f in server.pem server-key.pem; do
    if [ -d "$OUT_DIR/$f" ]; then
        echo "[gen-certs] $OUT_DIR/$f is a directory, not a file." >&2
        echo "[gen-certs] This happens when Docker auto-creates a bind-mount target for a" >&2
        echo "[gen-certs] single-file mount before the file exists (e.g. running 'docker" >&2
        echo "[gen-certs] compose up --no-deps ferryman' before certgen has ever run)." >&2
        echo "[gen-certs] Remove it and rerun 'docker compose up': rm -rf $OUT_DIR/server.pem $OUT_DIR/server-key.pem" >&2
        echo "[gen-certs] (it may be root-owned, since Docker created it: sudo rm -rf ... if so)" >&2
        exit 1
    fi
done

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
