#!/usr/bin/env bash
# CI check for deploy/k8s against a kind cluster that already has the image
# `ferryman:ci` loaded. Needs kubectl and curl.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../../.."

NS=ferryman-test
kubectl create namespace "$NS"
kubectl label namespace "$NS" pod-security.kubernetes.io/enforce=restricted
k() { kubectl -n "$NS" "$@"; }

dump() { k get pods,events -o wide || true; k logs deploy/ferryman --tail=50 || true; }
trap 'ec=$?; [ "$ec" = 0 ] || dump; exit "$ec"' EXIT

k apply -f deploy/k8s/test/upstream.yaml
# Same manifests as users get, with only the image swapped for the local build.
for f in configmap service; do k apply -f "deploy/k8s/$f.yaml"; done
sed 's#ghcr.io/bunty9/ferryman:0.3#ferryman:ci#' deploy/k8s/deployment.yaml | k apply -f -
k rollout status deploy/app --timeout=120s
k rollout status deploy/ferryman --timeout=120s

k port-forward svc/ferryman 18080:8080 >/dev/null &
PF=$!
trap 'kill $PF 2>/dev/null || true; ec=$?; [ "$ec" = 0 ] || dump; exit "$ec"' EXIT
code() { curl -s -o /dev/null -m 3 -w '%{http_code}' "$1" || true; }
for _ in $(seq 30); do [ "$(code http://127.0.0.1:18080/app/)" = 200 ] && break; sleep 1; done
[ "$(code http://127.0.0.1:18080/app/)" = 200 ] || { echo "/app not served"; exit 1; }
[ "$(code http://127.0.0.1:18080/extra/)" = 404 ] || { echo "/extra should not exist yet"; exit 1; }

# Add a route by editing the ConfigMap only; the pod must not restart.
before=$(k get pods -l app.kubernetes.io/name=ferryman -o jsonpath='{.items[*].metadata.uid}')
k get cm ferryman-config -o jsonpath='{.data.ferryman\.toml}' >/tmp/ferryman.toml
printf '\n[[routes]]\nprefix = "/extra"\nupstream = "http://app:8080"\n' >>/tmp/ferryman.toml
k create cm ferryman-config --from-file=ferryman.toml=/tmp/ferryman.toml \
  --dry-run=client -o yaml | k apply -f -

# kubelet sync period + cache TTL: allow up to 150 s (typically 60-90 s).
# The port-forward pins one pod; the other replica syncs on its own schedule,
# and we only need the forwarded one.
for i in $(seq 150); do
    [ "$(code http://127.0.0.1:18080/extra/)" = 200 ] && break
    sleep 1
done
[ "$(code http://127.0.0.1:18080/extra/)" = 200 ] || { echo "/extra not live after 150 s"; exit 1; }
echo "ConfigMap edit went live after ~${i}s"
after=$(k get pods -l app.kubernetes.io/name=ferryman -o jsonpath='{.items[*].metadata.uid}')
[ "$before" = "$after" ] || { echo "pods were replaced"; exit 1; }
k get pods -l app.kubernetes.io/name=ferryman \
  -o jsonpath='{range .items[*]}{.status.containerStatuses[0].restartCount}{"\n"}{end}' \
  | grep -qv '^0$' && { echo "container restarted"; exit 1; }
echo "kind check ok"
