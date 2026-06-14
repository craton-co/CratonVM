# 16 — X500 DN: RFC1779 spacing + dropped multi-valued RDN attributes

**Status:** FIXED — `native-builtins/src/jca/x500.rs`.
**Affected:** DefaultCertificateIdentityExtractorTest (`testX509SubjectCommonName`, now ✓).

## Symptom
`expected:<899700252580> but was:<null>` — keycloak's `X500NameRDNExtractor` couldn't find
the Common Name in a real X.509 cert subject. It does
`new X500Name(cert.getSubjectX500Principal().getName()).getRDNs(BCStyle.CN)`.

## Two root causes (isolated via apps/probe/kccert/AnsSubj.java on the real cert)
The ANS test cert's subject is `surname+givenName+CN, title, C` — the first RDN is
**multi-valued** (a SET of 3 `AttributeTypeAndValue`s).

1. **`getName()` returned RFC1779 spacing.** `render_canonical` joined RDNs with `", "`
   (comma+space). RFC 2253 (the default `X500Principal.getName()`, matching HotSpot) uses a
   bare comma. The space-prefixed `" CN"` then didn't match `getRDNs(BCStyle.CN)`.

2. **`decode_rdns` dropped all but the first attribute of a multi-valued RDN.** It took
   `attrs.into_iter().next()` — so `surname+givenName+CN` collapsed to just `SURNAME=…`,
   **losing the CN entirely**. CratonVM produced `SURNAME=KINE-CH,T=…,C=FR` where HotSpot has
   the full `…+CN=899700252580,…`.

## Fix
- `render_canonical` joins RDNs with `,` (RFC 2253/4514); `getName("RFC1779")` re-adds the
  `, ` spacing.
- `decode_rdns` renders a multi-valued RDN with its attributes joined by `+` (RFC 4514 §2.2),
  stored as one pre-rendered entry (empty key) that `render_canonical` passes through.

Result: `getSubjectX500Principal().getName()` →
`SURNAME=KINE-CH+GIVENNAME=NORBERT+CN=899700252580,T=…,C=FR`; `getRDNs(CN)` finds the CN.
DefaultCertificateIdentityExtractorTest 5/5.
