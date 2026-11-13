# t27_certs — throwaway TLS test fixtures

**These are NON-SECRET, throwaway key/certificate pairs used only by the
`t27_tls` self-tests and the in-tree TLS server harness.** They are generated
once for the test suite, are not used by any production or default code path to
protect real data, and carry no security value. They are committed so the TLS
tests build and run hermetically without a network fetch or an openssl
invocation at build time.

`server.key`, `server1.key`, `server2.key`, `client.key` are
`include_str!`-embedded by `../t27_tls.rs` (the `certs` module). The matching
`*.crt` / `ca.crt` are the self-signed certificates for the same key pairs.

Do **not**:
- reuse these keys for anything real (they are public — anyone can read them
  in the repo);
- treat a secret-scanner hit on this directory as a leaked credential — add a
  scanner allowlist entry for `native-builtins/src/t27_certs/**` instead.

To regenerate (if a test ever needs fresh material), produce a new self-signed
RSA keypair with openssl and replace the `.key` / `.crt` pair in place; keep
the CN / SAN values the `t27_tls` tests assert on.
