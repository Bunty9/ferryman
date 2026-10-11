# Security

## Reporting a vulnerability

Report vulnerabilities privately through GitHub: the repository's **Security**
tab, **Report a vulnerability** (see
[`SECURITY.md`](https://github.com/Bunty9/ferryman/blob/main/SECURITY.md) for
the policy, response times and supported versions). Do not open a public issue
for a suspected vulnerability.

## Verifying what you run

Release archives from 0.2.3 on, and the GHCR image, carry GitHub build
provenance attestations:

```bash
gh attestation verify ferryman-v$v-$t.tar.gz --repo Bunty9/ferryman
gh attestation verify oci://ghcr.io/bunty9/ferryman:X.Y.Z --repo Bunty9/ferryman
```

An image pushed by the normal release is signed by `release.yml`; one recovered
after a failed release job is signed by `image-publish.yml`. Verify with
`--repo` as above, and add `--signer-workflow
Bunty9/ferryman/.github/workflows/<release|image-publish>.yml` to pin one.

Releases also publish `SHA256SUMS` and a `.sha256` file per archive
([Quickstarts](quickstarts.md#release-archives)). Tier-1 binaries embed their
dependency list (`cargo auditable`), so `cargo audit bin ./ferryman` works on
them. Dependencies are checked in CI with `cargo-deny`, and workflows are
SHA-pinned and linted with zizmor.

## Runtime posture

- **Non-root image.** `FROM scratch`, uid/gid 65532, no writable paths
  (`--read-only` works). Mounted files must be readable by 65532.
- **Restricted Pod Security Standard** manifests in `deploy/k8s/`; a hardened
  systemd unit in `deploy/systemd/` (`DynamicUser`, `ProtectSystem=strict`,
  `CAP_NET_BIND_SERVICE` only, `@system-service` syscalls).
- **Admin listener** is unauthenticated: never expose port 9090 publicly.
- **Forwarded headers** from untrusted peers are replaced or stripped
  ([Forwarded headers](forwarded-headers.md)).
- **Path handling**: ambiguous or traversal-shaped paths are rejected with
  400 before routing, and the forwarded path is never rewritten
  ([Routing](routing.md)). Routes are not access control.
- **TLS** is rustls (`ring` provider); a bad certificate on reload keeps the
  previous one. Upstream connections are plain HTTP: run ferryman close to
  its upstreams.
- Logs never include query strings.
