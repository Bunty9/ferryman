# Forwarded headers & trusted_proxies

What the upstream sees depends on whether the connecting peer is in
`trusted_proxies` (an empty list means every peer is untrusted). Entries are
CIDR ranges or bare IPs, IPv4 and IPv6; host bits are ignored (`10.0.0.1/8` =
`10.0.0.0/8`); IPv4 clients (also IPv4-mapped IPv6 peers) match only IPv4
ranges, so `::/0` does not trust them. The list hot-reloads and is applied
per request.

| Header | Untrusted peer | Trusted peer |
| --- | --- | --- |
| `X-Forwarded-Proto` | set from the connection | incoming kept (rightmost value); set from the connection if absent |
| `X-Forwarded-Port` | client values stripped; set to the listener's port | rightmost incoming value kept if a valid port, else the listener's port |
| `X-Forwarded-For` | peer IP appended (`xff = "replace"`: only the peer) | peer IP appended (`xff = "replace"`: only the derived client) |
| `X-Real-IP` | overwritten with the peer IP | always overwritten: the rightmost `X-Forwarded-For` entry that is not a trusted proxy (`ip:port` / `[v6]:port` tolerated; an unparsable entry stops the walk), else the peer |
| `Forwarded` | stripped; with `forwarded_header = true` a fresh `for=peer;proto;host` element | kept; with `forwarded_header = true` the element is appended |
| `X-Forwarded-Host` | stripped | kept |
| `X-Forwarded-Ssl` | stripped | kept |
| `X-Forwarded-Scheme` | stripped | kept |
| `X-Forwarded-Prefix` | stripped; set to the prefix on `strip_prefix` routes | kept; on `strip_prefix` routes a single plain-path value is prepended (`/outer` + `/api` = `/outer/api`), anything else is replaced |

Request headers that differ from a trust-governed header only by `_` for `-`
(for example `X_Real_IP`) are removed for every peer.

## Details

- ferryman does not add `X-Forwarded-Host` itself; the `Host` header is
  unchanged.
- In `Forwarded`, `for=` is the connecting peer, `proto=` is this hop's
  connection scheme and `host=` is the client's own host (URI authority, else
  `Host`), also with `rewrite_host = true`.
- `X-Forwarded-Port` is never derived from `Host`. Without a known listener
  port (the deprecated `proxy::handle`, or `handle_streaming` called
  directly) it is omitted for untrusted peers.
- If `trusted_proxies` is empty and a request arrives from a private,
  loopback or link-local peer, ferryman logs one warning per process
  suggesting `trusted_proxies`, in case it sits behind a proxy or load
  balancer.

## Behind another proxy or load balancer

Add that hop's address range to `trusted_proxies`; otherwise its
`X-Forwarded-*` values are replaced or stripped and your app sees ferryman's
view of the connection. Before 0.2.3 clients could forge `X-Real-IP`,
`Forwarded` and `X-Forwarded-Host`.

**Only trust a hop that overwrites or strips `Forwarded` and
`X-Forwarded-Host`.** AWS ALB and most cloud load balancers pass
client-supplied values of these through, so downstream apps should not trust
those headers behind such a load balancer. (`X-Real-IP` is safe either way:
ferryman derives it itself.)

## Vaultwarden

Use ferryman 0.2.3 or later. It sets `X-Real-IP` from the TCP peer (or,
behind a trusted proxy, from `X-Forwarded-For`), so Vaultwarden's default
`IP_HEADER=X-Real-IP` is correct. On 0.2.2 the only non-spoofable setting is
`IP_HEADER=none`, which makes all clients share ferryman's IP for rate
limiting.
