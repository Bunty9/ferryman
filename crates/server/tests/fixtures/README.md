Test-only self-signed certificates and keys (100 years). Not secrets.
- test-cert.pem / test-key.pem (CN=localhost): crates/server/tests/cli.rs (`ferryman check` with TLS) and the TLS reload tests.
- test-cert-2.pem / test-key-2.pem (CN=localhost-2): the "rotated" pair in crates/server/tests/reload.rs.
Regenerate with: openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout test-key-2.pem -out test-cert-2.pem -days 36500 -subj "/CN=localhost-2" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
