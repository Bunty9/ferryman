# Kubernetes

Namespace-agnostic manifests; apply them into any namespace:

```sh
kubectl create namespace gateway
kubectl -n gateway apply -f deploy/k8s/
```

(`test/` holds the CI-only stub upstream and is not applied by the command
above.) Edit the routes in `configmap.yaml` first; the sample routes `/app` to
a Service named `app` in the same namespace.

- **Restricted Pod Security Standard.** Non-root 65532, read-only root
  filesystem, no privilege escalation, all capabilities dropped,
  `RuntimeDefault` seccomp. Label the namespace
  `pod-security.kubernetes.io/enforce=restricted` and it is admitted.
- **Config reload without restart.** The ConfigMap is mounted as a
  directory at `/etc/ferryman` (never `subPath`, which does not update).
  `kubectl edit configmap ferryman-config` and the kubelet syncs the files
  within its sync period (typically up to ~60-90 s); ferryman picks up the
  `..data` symlink swap. A bad config is logged and the old one stays live.
  CI asserts this on a kind cluster.
- **Probes.** `readinessProbe` -> admin `/readyz` (goes 503 on SIGTERM; not tied to
  upstream health), `livenessProbe` -> `/healthz`, both on the `admin` port.
- **Shutdown.** Endpoint removal takes several seconds to reach proxies and
  ingress controllers, so the config sets `shutdown_delay_secs = 10` (keep
  serving, `/readyz` 503) then `drain_timeout_secs = 15`;
  `terminationGracePeriodSeconds: 35` exceeds the sum. Raise all three
  together.
- **Admin port.** `9090` serves metrics and probes; the Service exposes it
  in-cluster only. Do not route it through an Ingress; add a NetworkPolicy
  if needed.
- **TLS (optional).** Create a `kubernetes.io/tls` Secret `ferryman-tls` and
  uncomment the `--tls-*` args, the `tls` volume and its mount in
  `deployment.yaml`. `fsGroup: 65532` makes the files readable. A renewed
  certificate (e.g. cert-manager) is picked up on reload like the config.
- **Behind an ingress/LB.** Add its CIDR to `trusted_proxies` in the ConfigMap
  ([Forwarded headers](https://bunty9.github.io/ferryman/forwarded-headers.html)).
