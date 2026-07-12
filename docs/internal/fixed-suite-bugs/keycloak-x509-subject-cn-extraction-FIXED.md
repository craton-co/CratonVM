# Keycloak X.509 Subject CN extraction — fixed

Status: fixed 2026-07-12

## Root cause

Keycloak's Elytron provider builds an `X500AttributePrincipalDecoder` for the
CN OID (`2.5.4.3`). That decoder reads the DER returned by
`X500Principal.getEncoded()`.

CratonVM already rendered a multi-valued RDN from a loaded certificate as,
for example, `OU=Keycloak+CN=899700252580`. Its X.500 re-encoder then parsed
that complete string as a single `OU` attribute instead of a grouped RDN.
The regenerated DER therefore no longer contained a CN attribute, and Elytron
returned `null`.

`native-builtins/src/jca/x500.rs` now parses unescaped `+` separators,
preserves each grouped attribute, DER-sorts the SET elements, and re-encodes
the complete RDN. Escaped separators remain part of their attribute value.

## Validation

- Built `cratonvm-keycloak-x509-subject-cn-fixed-20260712` on the Azure host
  from this worktree with an isolated target directory.
- Generated an OpenSSL PEM with the CN `899700252580` in a multi-valued RDN
  (`OU=Keycloak+CN=899700252580`).
- Exercised the exact Keycloak Elytron dependency path:
  `X500AttributePrincipalDecoder("2.5.4.3").apply(cert.getSubjectX500Principal())`.
  It returned `899700252580` under both `--nojit` and JIT.
- Ran `jca::x500::tests::grouped_rdn_preserves_each_attribute_in_der`:
  `1 passed; 0 failed`.

The original Keycloak test expectation is the same CN value:
`ElytronCertificateIdentityExtractorTest::testX509SubjectCommonName` expects
`899700252580` after calling `getX500NameExtractor("CN", subject)`.
