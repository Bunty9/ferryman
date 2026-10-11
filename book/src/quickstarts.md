# Quickstarts

Every recipe below needs a config file. The smallest useful one:

```toml
[[routes]]
prefix = "/app"
upstream = "http://app:8000"
```

Validate any config without binding a port:

```bash
ferryman check --config ferryman.toml     # prints "config ok: 1 routes"
```

Defaults: proxy on `0.0.0.0:8080` (or `0.0.0.0:$PORT`), admin listener on
`0.0.0.0:9090`.

## Install

### Docker (GHCR)

Multi-arch (linux/amd64, linux/arm64) images are published to GHCR from the
release binaries, with build provenance attached:
`ghcr.io/bunty9/ferryman:X.Y.Z`, `:X.Y` and `:latest` (prereleases get only
the exact tag).

```bash
docker run --rm --read-only -p 8080:8080 -p 127.0.0.1:9090:9090 \
  -v "$PWD/config:/etc/ferryman:ro" \
  ghcr.io/bunty9/ferryman:latest --config /etc/ferryman/ferryman.toml
```

The image is `FROM scratch` and runs as **uid/gid 65532** (non-root) with no
writable paths, so `--read-only` works. Mounted config, TLS certificate and
key files must be readable by 65532: mode `0644`, or group-readable plus
`--group-add <gid>` (compose `group_add`, Kubernetes `fsGroup`). A `0600`
file owned by your own user is not readable and the container fails at
startup with a permission error. Mount the config as a **directory** so hot
reload sees rename-replace saves. Ports below 1024 need
`net.ipv4.ip_unprivileged_port_start` lowered outside Docker's defaults.

The image has a built-in `HEALTHCHECK` (`ferryman healthcheck`); there is no
shell or curl in it.

### cargo binstall / cargo install

```bash
cargo binstall ferryman          # downloads the release archive (releases after 0.2.1)
cargo install --locked ferryman  # builds from crates.io (Rust 1.88+)
```

### Release archives

Prebuilt binaries are attached to every
[GitHub release](https://github.com/Bunty9/ferryman/releases) as
`ferryman-v<version>-<target>.tar.gz` (`.zip` on Windows), each holding the
binary, README, CHANGELOG, both licenses and an example `config.toml`.
Required targets: `x86_64`/`aarch64-unknown-linux-musl` (static),
`x86_64`/`aarch64-apple-darwin`, `x86_64-pc-windows-msvc`. Best effort (may
be missing from a release): `armv7`/`arm-unknown-linux-musleabihf`,
`i686-unknown-linux-musl`, `riscv64gc-unknown-linux-gnu`,
`aarch64-pc-windows-msvc`, `x86_64-unknown-freebsd`.

```bash
v=0.3.0   # or the latest release
t=x86_64-unknown-linux-musl
base=https://github.com/Bunty9/ferryman/releases/download/v$v
curl -fsSLO $base/ferryman-v$v-$t.tar.gz -O $base/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS      # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
tar xzf ferryman-v$v-$t.tar.gz
./ferryman-v$v-$t/ferryman --version
```

Each archive also has a standalone `<archive>.sha256` file. See
[Security](security.md) for attestation verification.

On Windows, Ctrl-C drains gracefully; console close or system shutdown
starts the drain, but Windows terminates the process after about 5 s.

## Try it locally

```bash
ferryman --config config.toml

# Optional: local upstreams that answer 200 on every path (from a checkout).
cargo run --release -p ferryman --example echo_upstream -- 127.0.0.1:8001 &

curl -i http://localhost:8080/svc-a/hello
# HTTP/1.1 502 bad gateway          (no upstream listening yet)
# HTTP/1.1 503 upstream unavailable (once its circuit has opened)
curl -i http://localhost:8080/no-such-route
# HTTP/1.1 404 no route

# Optional TLS termination (HTTP/2 via ALPN):
ferryman --config config.toml --tls-cert cert.pem --tls-key key.pem

curl -s http://localhost:9090/metrics | head
```

Edit `config.toml` while ferryman runs: the routing table reloads atomically
without dropping live connections.

## systemd

[`deploy/systemd/ferryman.service`](https://github.com/Bunty9/ferryman/blob/main/deploy/systemd/ferryman.service)
is a hardened unit (`DynamicUser`, `ProtectSystem=strict`, only
`CAP_NET_BIND_SERVICE`, `@system-service` syscalls).

```bash
sudo install -m755 ferryman /usr/local/bin/ferryman
sudo install -d /etc/ferryman && sudo cp config.toml /etc/ferryman/
sudo cp deploy/systemd/ferryman.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ferryman
```

- **Notify.** With `NOTIFY_SOCKET` set (`Type=notify`) ferryman sends
  `READY=1` once the proxy and admin listeners are bound, `RELOADING=1` +
  `MONOTONIC_USEC` / `READY=1` around each SIGHUP reload (also when the
  reload is rejected and the old config is kept), and `STOPPING=1` when
  shutdown begins. It is a no-op when the variable is unset. File-watch
  reloads are not announced; watchdog (`WATCHDOG=1`) is not implemented.
- **Reload.** `systemctl reload ferryman` runs `kill -HUP $MAINPID`. On
  systemd 253+ you can use `Type=notify-reload` instead and drop
  `ExecReload`.
- **TLS.** `LoadCredential=` copies the cert and key into a private
  directory; the unit passes them as `--tls-cert %d/tls.crt --tls-key
  %d/tls.key`. Renewing `/etc/ferryman/tls.*` takes effect with
  `systemctl reload`.
- **Stopping.** `TimeoutStopSec=35s` covers the default 25 s drain; raise it
  if you raise `shutdown_delay_secs` or the drain.
- **Logs.** JSON on stdout lands in the journal:
  `journalctl -u ferryman -o cat | jq .` (or `--log-format text`).

## Kubernetes

[`deploy/k8s/`](https://github.com/Bunty9/ferryman/tree/main/deploy/k8s) has
a Deployment, Service and ConfigMap that pass the Restricted Pod Security
Standard. A `kind` CI job applies them and asserts that a ConfigMap edit goes
live without a restart.

```bash
kubectl create namespace gateway
kubectl -n gateway apply -f deploy/k8s/
```

Edit the routes in `configmap.yaml` first; the sample routes `/app` to a
Service named `app` in the same namespace.

- **Restricted PSS.** Non-root 65532, read-only root filesystem, no
  privilege escalation, all capabilities dropped, `RuntimeDefault` seccomp.
- **Config reload without restart.** The ConfigMap is mounted as a
  *directory* at `/etc/ferryman` (never `subPath`, which the kubelet never
  updates). After `kubectl edit configmap ferryman-config` the kubelet syncs
  within its sync period (typically up to 60-90 s); ferryman picks up the
  `..data` symlink swap. A bad config is logged and the old one stays live.
  Point `--config` at the mount's top-level file, not at
  `.../..data/config.toml`.
- **Probes.** `readinessProbe` -> admin `/readyz` (503 on SIGTERM; not tied to
  upstream health), `livenessProbe` -> `/healthz`, both on the `admin` port.
- **Shutdown.** Endpoint removal takes several seconds to reach proxies and
  ingress controllers, so the config sets `shutdown_delay_secs = 10` then
  `drain_timeout_secs = 15`; `terminationGracePeriodSeconds: 35` exceeds the
  sum. Raise all three together. The scratch image has no `sleep`, so an exec
  `preStop` hook is not an option; newer clusters have the native
  `lifecycle.preStop.sleep`.
- **Admin port.** `9090` serves metrics and probes; the Service exposes it
  in-cluster only. Do not route it through an Ingress; add a NetworkPolicy if
  needed.
- **TLS (optional).** Create a `kubernetes.io/tls` Secret `ferryman-tls` and
  uncomment the `--tls-*` args, the `tls` volume and its mount in
  `deployment.yaml`. `fsGroup: 65532` makes the files readable. A renewed
  certificate (e.g. cert-manager) is picked up on reload like the config.
- **Behind an ingress/LB.** Add its CIDR to `trusted_proxies` in the
  ConfigMap ([Forwarded headers](forwarded-headers.md)).

## Docker Compose

Minimal:

```yaml
services:
  ferryman:
    image: ghcr.io/bunty9/ferryman:latest
    read_only: true
    command: ["--config", "/etc/ferryman/ferryman.toml"]
    volumes: ["./config:/etc/ferryman:ro"]
    ports: ["8080:8080", "127.0.0.1:9090:9090"]
```

The full reference deployment is
[`examples/full-stack/`](https://github.com/Bunty9/ferryman/tree/main/examples/full-stack):
ferryman with TLS, three demo upstreams, Prometheus with alert rules, and a
Grafana dashboard, driven by an asserted tour (`bash
examples/full-stack/demo.sh --ci`, run in CI).

## PaaS and orchestrators

Platforms that inject `PORT` need no flags. With `FERRYMAN_CONFIG_TOML` the
whole config is an environment variable:

```bash
PORT=10000 FERRYMAN_CONFIG_TOML='[[routes]]
prefix = "/"
upstream = "http://127.0.0.1:3000"' ferryman
```

It listens on `0.0.0.0:10000`. Inline config is not watched, so it is not
hot-reloaded: change the variable and restart. Platforms that only probe the
serving port should set `local_health_path` (for example `"/up"`): the proxy
answers `200 ok` itself, with no upstream and no breaker involved. See
[Operations](operations.md#probes). A CI test (`crates/server/tests/paas.rs`)
covers the profiles below.

- **Cloud Run** (SIGTERM, then SIGKILL after 10 s): `shutdown_delay_secs = 0`,
  `drain_timeout_secs` at most 8. It probes the serving port, so set
  `local_health_path` if you use probes.
- **Render, Railway, Heroku**: all set `PORT`. Heroku sends SIGTERM and kills
  after 30 s (the default 25 fits); Render and Railway are configurable. Use
  inline config or a file committed in the image.
- **Fly.io**: [`fly.toml`](https://github.com/Bunty9/ferryman/blob/main/fly.toml)
  publishes only the proxy (`internal_port` 8080); the admin listener
  (`[::]:9090`, dual-stack because Fly's private network is IPv6) is never
  exposed. Fly's Prometheus scrapes `9090/metrics` (`[metrics]`) and a
  machine check probes `9090/healthz`. To probe the proxy port instead, set
  `local_health_path = "/up"` and point the check at it. `kill_signal =
  "SIGINT"` with `kill_timeout = "30s"` covers the default 25 s drain.
- **ECS behind an ALB**: the target group's deregistration delay already keeps
  the task in service before SIGTERM is sent, so `shutdown_delay_secs` is
  usually unnecessary. Keep `stopTimeout` above delay + drain. Set
  `keepalive_timeout_secs` for the ALB ([Timeouts](timeouts.md#behind-a-load-balancer)).
  Container health check: `"healthCheck": {"command": ["CMD",
  "/usr/local/bin/ferryman", "healthcheck"]}`.
