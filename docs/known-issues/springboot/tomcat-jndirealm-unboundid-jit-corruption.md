# Tomcat JNDIRealmIntegration UnboundID cross-package JIT corruption

**Status:** OPEN -- contained by the conservative JIT policy on 2026-07-23.

**Affected test:** `org.apache.catalina.realm.TestJNDIRealmIntegration`.

The real-JDK Tomcat LDAP matrix is correct with `--nojit` and on HotSpot, but
the default CratonVM JIT intermittently loses a live `String` used by the
embedded UnboundID LDAP server's DN/RDN matching path. The observable symptom
is an all-zero receiver header at `String.equals(Object)`, followed either by
an authentication-group assertion failure or an access violation.

The earlier `RDN.getNameValuePairs` JIT guard addresses a separate
special-character credential residual. It does not prevent this one.

## Bisection evidence

- Baseline JIT: 2 failures in 5 sequential runs on 2026-07-23.
- `--nojit`: passes the same class.
- `CRATONVM_JIT_BISECT_ONLY=com/unboundid/`: 4 non-passing runs out of 5
  (three assertion failures and one crash).
- The individual `com/unboundid/ldap/sdk/`,
  `com/unboundid/ldap/matchingrules/`, `com/unboundid/asn1/`, and
  `com/unboundid/util/` slices each passed five runs. The producer therefore
  spans compiled UnboundID packages; it is not an LDAP protocol or native-JNDI
  implementation defect.

## Containment and follow-up

`../../../vm/src/jit/skip_list.rs` keeps `com/unboundid/` interpreted under the
conservative policy. It can be lifted only for diagnosis with:

```text
CRATONVM_JIT_ALLOW_PACKAGES=com/unboundid/
```

Do not move this record to `../../internal` until a narrower compiled producer
is identified and the repeated JIT matrix passes without the package guard.
