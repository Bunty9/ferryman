# Task A1 report: HTTP/1 smuggling suite

File: crates/server/tests/smuggling.rs (11 tests, 10 run + 1 ignored FINDING; runtime ~2.3s). Docs: book/src/security.md, CHANGELOG [Unreleased] Added.
Upstream = raw TcpListener recording exact bytes. Behaviour is hyper 1.11 / httparse 1.10.

| Input | Client outcome | Upstream bytes | Verdict |
|---|---|---|---|
| CL+TE (chunked, CL 4) + trailing GET /evil | 200, `connection: close` | one POST, TE chunked, no CL, no evil | OK (TE wins) |
| dup CL differing (3,5) | 400 | none | OK |
| CL `+3`, `3 3`, `0x3`, `3, 3`; `content-length : 3` | 400 | none | OK |
| dup CL equal | 200 | one CL | OK (normalised) |
| TE `chunked, identity`, `xchunked`, duplicated TE | 400 | none | OK |
| TE `chunked ` / `CHUNKED` | 200 | `transfer-encoding: chunked` | OK (normalised) |
| TE on HTTP/1.0 | 400 | none | OK |
| obs-fold (value, first line) | 400 | none | OK |
| bare LF (all / mixed) | 200 | CRLF re-serialised, 1 request | OK (normalised) |
| NUL / CR in value; space before colon | 400 | none | OK |
| chunk size overflow / negative / non-hex / bare-LF ext / missing chunk CRLF | 400 (body error) | no complete request, no evil | OK |
| chunk ext `3;a=b;c` | 200 | ext dropped | OK |
| missing final CRLF after 0 chunk | aborted | body not terminated | OK |
| absolute-form mismatched Host | 200 | host = URI authority | OK |
| `GET ... HTTP/1.1 junk` | 400 | none | OK |
| lowercase / unknown method | 200 | passed through as-is | OK |
| 70KB URI / 1MB header | 414 / 431 or 400 | none | OK |
| two pipelined GETs | 2x200 in order | 2 requests in order | OK |
| POST CL 3 + pipelined GET | 2x200 | 2 requests, exact framing | OK |
| CL 1 but 3 body bytes + GET | 2x200 | request 2 has method `bcGET` (CL-exact framing) | OK, documented |
| Upgrade: websocket | 501 | none | OK |
| Upgrade + pipelined GET /evil | 501 then 200 | exactly one clean GET /evil, no upgrade headers | FINDING (low) |
| Expect 100-continue + Upgrade, body eager / withheld | 501, no 100 | next request parsed correctly / evil never forwarded | OK |

## Finding (low): `upgrade_pipelined_bytes_never_reach_upstream` is `#[ignore]`d
After the 501 for an Upgrade request the connection stays open, so a request pipelined behind it is parsed normally and forwarded (well-formed, singly framed, not a desync). The brief wants pipelined bytes after Upgrade not to reach upstream; that is not true today. Suggested fix for the controller: send `connection: close` on the 501 for Upgrade/CONNECT. Current behaviour is pinned by `upgrade_then_pipelined_request_is_a_single_clean_request`; B1 will update both.

## Gate
fmt ok; clippy -D warnings --locked ok; cargo test --workspace --locked all green (smuggling 2.3s). `cargo deny check`: advisories FAILED on RUSTSEC-2026-0346/0347 (rustls, new advisories; no dependency changed by this task), bans/licenses/sources ok.
