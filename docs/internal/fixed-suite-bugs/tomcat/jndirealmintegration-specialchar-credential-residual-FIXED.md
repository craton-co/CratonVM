# TestJNDIRealmIntegration — special-character credential authentication residual (FIXED)

**Status:** FIXED on `codex/fix-jndirealm-specialchar-closure-20260715-001`.

## Symptom

With the real JDK and real sockets, the JIT-on run of
`org.apache.catalina.realm.TestJNDIRealmIntegration` previously failed 15 of
76 parameter cases with a null `GenericPrincipal` assertion:

- eleven cases using the RFC 4514-special-character username and password
  `<>+="#;,rrr`; and
- four cases using `testsub` under an escaped-semicolon OU.

HotSpot and CratonVM with `--nojit` both passed all 76 cases, proving this was
a JIT-only runtime defect rather than an LDAP fixture or Tomcat configuration
gap.

## Root cause and fix

The failures share UnboundID's in-memory LDAP DN/RDN matching path. JIT
package bisection narrowed the producer to `com/unboundid/ldap/sdk/`; method
bisection then showed that interpreting only
`com.unboundid.ldap.sdk.RDN.getNameValuePairs()` restores the complete matrix.
The neighbouring RDN comparison and normalisation methods remain eligible.

`vm/src/jit/skip_list.rs` now keeps that single accessor interpreted under the
default conservative JIT policy. The guard is deliberately narrow and can be
lifted for future backend diagnosis with:

```text
CRATONVM_JIT_ALLOW_PACKAGES=com/unboundid/ldap/sdk/
```

The JIT code-generation defect in this array-backed `SortedSet` return path is
therefore contained without disabling JIT for the LDAP client, the UnboundID
SDK generally, or Tomcat.

## Validation

All runs used the preserved Tomcat suite classpath, real JDK 25, and the
real-network socket path (`CRATONVM_REAL_NET_SOCKETS=1`):

| Runtime/configuration | Result |
| --- | --- |
| HotSpot | `OK (76 tests)` |
| CratonVM before the fix, JIT on | `Tests run: 76, Failures: 15` |
| CratonVM before the fix, `--nojit` | `OK (76 tests)` |
| CratonVM after the fix, default JIT | `OK (76 tests)` |

Focused policy coverage also passes:

```text
cargo test --release -p cratonvm-vm unboundid_rdn_name_value_pairs --lib
# 2 passed; 0 failed
```

This closes both the special-character credential group and the escaped-OU
residual group. The earlier connection-level JNDI LDAP fix remains documented
in [jndirealmintegration-ldap-connection-npe-FIXED.md](jndirealmintegration-ldap-connection-npe-FIXED.md).

## Diagnostic follow-up (2026-07-16)

The JIT-on class continued to complete all 76 cases, but normal runs printed
bounded GC "corrupt header" messages. Allocation breadcrumbs proved those
addresses were interior conservative-root candidates in valid allocations,
which the collector was already safely rejecting without marking or scanning.
The A2 breadcrumb report is now emitted only when its documented
`CRATONVM_DBG_A2=1` forensic switch is enabled. Default JIT runs remain quiet;
the opt-in mode retains the candidate context for future collector diagnosis.
