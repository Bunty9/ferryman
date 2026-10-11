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
| `X-Forwarded-Port` | client values stripped; none added | rightmost incoming value kept if a valid port, else none |
| `X-Forwarded-For` | peer IP appended (`xff = "replace"`: only the peer) | peer IP appended (`xff = "replace"`: only the derived client) |
| `X-Real-IP` | overwritten with the peer IP | always overwritten: the rightmost `X-Forwarded-For` entry that is not a trusted proxy (`ip:port` / `[v6]:port` tolerated; an unparsable entry stops the walk), else the peer |
| `Forwarded` | stripped; with `forwarded_header = true` a fresh `for=peer;proto;host` element | kept; with `forwarded_header = true` the element is appended |
| `X-Forwarded-Host` | stripped | kept |
| `X-Forwarded-Ssl` | stripped | kept |
| `X-Forwarded-Scheme` | stripped | kept |
| `X-Forwarded-Prefix` | stripped; set to the prefix on `strip_prefix` routes | kept; on `strip_prefix` routes a single plain-path value is prepended (`/outer` + `/api` = `/outer/api`), anything else is replaced |

Request headers whose name maps onto a trust-governed one when every
non-alphanumeric byte is read as `-` (runs collapsed, ends trimmed; for
example `X_Real_IP`, `X.Real.IP`) are removed for every peer.

## Details

- ferryman never sets `X-Forwarded-Host`. The `Host` header is forwarded
  unchanged unless the route has `rewrite_host = true`, which replaces it with
  the upstream's authority; the client's host is then lost to the app unless
  you set `forwarded_header = true` (its `host=` carries it).
- In `Forwarded`, `for=` is the connecting peer, `proto=` is this hop's
  connection scheme and `host=` is the client's own host (URI authority, else
  `Host`), also with `rewrite_host = true`.
- ferryman never writes `X-Forwarded-Port`: not from the listener port (with
  port mapping such as 80 to 8080 that would make apps build `:8080` URLs) and
  not from `Host`. It is only passed through from a trusted peer.
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
