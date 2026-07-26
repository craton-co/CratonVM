# Keycloak CredentialModel lazy-init getter JIT mitigation

**Status:** FIXED/MITIGATED on 2026-07-01 for the suite-visible Keycloak failure.

The Keycloak `CredentialModelTest` failure is now contained by a targeted JIT
skip-list entry for the two real lazy-init getters that previously returned
`null` under CratonVM JIT. Full `KcRunner` verification passes in both JIT and
`--nojit` modes.

This is a containment fix, not proof that the broader context-sensitive
single-pass backend issue is fully root-caused. If the underlying backend family
is investigated later, use the liftable skip-list entries and the real Keycloak
classpath described below.

## Verified Result

Verified with a unique CratonVM binary built from the fixing tree:

```powershell
$KC = 'C:\craton\CratonVM\apps\keycloak'
$CP = "$KC\kc-runner;" + (Get-Content "$KC\kc-universal-cp.txt" -Raw).Trim()
$CV = 'C:\craton\CratonVM-codex-keycloak-credential-verify-20260701-1\cvkcverify-20260701-1.exe'

& $CV --java-home 'C:\Program Files\Java\jdk-25' --stack-dump-on-timeout 0 `
    -cp $CP KcRunner org.keycloak.models.credential.CredentialModelTest

& $CV --java-home 'C:\Program Files\Java\jdk-25' --nojit --stack-dump-on-timeout 0 `
    -cp $CP KcRunner org.keycloak.models.credential.CredentialModelTest
```

Results:

| Mode | Result |
|------|--------|
| CratonVM, JIT default | `tests=5 failed=0 aborted=0 skipped=0 containersFailed=0` |
| CratonVM, `--nojit` | `tests=5 failed=0 aborted=0 skipped=0 containersFailed=0` |

The remaining warning about
`org.keycloak.testframework.ServerConfigClassOrderer` being absent is a
classpath ordering warning from the harness and does not fail the test run.

## Mitigation

`../../../../vm/src/jit/skip_list.rs` keeps only these two getters interpreted under
`SkipPolicy::Conservative`:

- `org/keycloak/models/credential/dto/PasswordCredentialData.getAdditionalParameters`
- `org/keycloak/models/credential/dto/PasswordSecretData.getAdditionalParameters`

The surrounding `CredentialModel.getPasswordCredentialData` path remains
JIT-eligible, so this is not a blanket Keycloak credential de-JIT.

The entries remain debuggable/liftable:

- `SkipPolicy::Aggressive` allows these methods to JIT.
- `CRATONVM_JIT_ALLOW_PACKAGES=org/keycloak/models/credential/` allows targeted
  bisection without editing the skip-list.

The focused guard is:

```powershell
cargo test -p cratonvm-vm keycloak_credential_lazy_init_getters_are_interpreted -- --nocapture
```

## Original Symptom

`org.keycloak.models.credential.CredentialModelTest.canCreateDefaultCredentialModel`
failed under JIT:

```text
java.lang.AssertionError:
Expected: <{}>
     but: was null
```

The failing value was
`model.getPasswordCredentialData().getAdditionalParameters()` and/or the matching
secret-data getter. Both getters have the standard lazy-init shape:

```java
public MultivaluedHashMap<String, String> getAdditionalParameters() {
    if (additionalParameters == null) {
        additionalParameters = new MultivaluedHashMap<>();
    }
    return additionalParameters;
}
```

A correct JVM cannot return `null` from this method after the store. HotSpot and
CratonVM `--nojit` returned `{}`.

## Investigation Trail

The original escape-analysis/scalar-replacement hypothesis was refuted on
2026-06-21:

- `new MultivaluedHashMap<>()` is not an elidable construction in CratonVM's
  current scalar-replacement path because it extends `HashMap`, not a direct
  canonical `Object` subclass.
- The getter therefore does not reach the scalar-new IR path that was originally
  suspected.
- Standalone negative controls for the bare lazy-init shape stayed green under
  both JIT backends and `--nojit`.

The bug needed the real Keycloak classes and the surrounding Jackson
deserialization caller context. That points at a context-sensitive single-pass
`getfield`/`putfield`/`areturn` or register/GC-root interaction rather than the
bare lazy-init pattern.

During the same investigation, a separate defensive escape-lattice bug was found
and fixed: `Op::Param` nodes now initialize to `GlobalEscape`, matching the
store-publish rule's invariant. That hardening does not explain this Keycloak
failure, but it closed a real latent invariant gap.

## If This Regresses

Re-open this only if the real `KcRunner`
`org.keycloak.models.credential.CredentialModelTest` fails again with the
skip-list enabled. For broader backend root-cause work, lift the entries with
`CRATONVM_JIT_ALLOW_PACKAGES=org/keycloak/models/credential/` and capture
single-pass disassembly for the two `getAdditionalParameters` methods under the
real Keycloak classpath:

- `../../../../apps/keycloak/kc-universal-cp.txt`
- `../../../../apps/keycloak/kc-runner`
