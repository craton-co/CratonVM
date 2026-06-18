# JIT miscompile — lazy-init getter returns `null` (CredentialModelTest)

**Status:** OPEN — CratonVM-only, **JIT-only** (passes `--nojit`). Handoff from the keycloak
full-suite run (kcfull-2026-06-18 report 18, "Root C").
**Severity:** correctness. A `new`-stored-to-field-then-returned object is dropped under JIT,
so a lazy-init accessor returns `null` where it can never legitimately do so.

## Symptom
`org.keycloak.models.credential.CredentialModelTest.canCreateDefaultCredentialModel` fails:

```
java.lang.AssertionError:
Expected: <{}>
     but: was null
```

The failing value is `model.getPasswordCredentialData().getAdditionalParameters()` (and/or the
matching `PasswordSecretData` getter). Both have the standard lazy-init shape:

```java
public MultivaluedHashMap<String, String> getAdditionalParameters() {
    if (additionalParameters == null) {
        additionalParameters = new MultivaluedHashMap<>();
    }
    return additionalParameters;          // <-- returns null under JIT
}
```

This getter can **never** return `null` on a correct JVM. HotSpot returns `{}`.

## Confirmed JIT-only
| Mode | Result |
|------|--------|
| CratonVM, JIT (default) | `tests=5 failed=1` (this method) |
| CratonVM, `--nojit`     | `tests=5 failed=0` ✅ |
| HotSpot                 | `tests=5 failed=0` ✅ |

A standalone single-method repro reproduces the *path* but **not** the failure:

```java
static class Box { Object f; Object get(){ if(f==null){ f=new Object(); } return f; } }
// 500k iterations of: new Box().get()  →  nulls=0 on CratonVM JIT too
```

So the miscompile is **not** triggered by the bare lazy-init shape — it depends on the actual
method/caller context (the `MultivaluedHashMap` allocation, the surrounding Jackson
deserialization call chain that produces the `PasswordCredentialData`, and/or the specific
register/escape state at the `getfield`/`putfield`/`areturn`).

## Likely root cause
The `new MultivaluedHashMap()` escapes via `putfield additionalParameters` (store to an
instance field) **and** `areturn`. If the JIT escape-analysis pass
(`vm/src/jit/x64.rs::analyze_escapes`) fails to treat the value as escaping — i.e. it
scalar-replaces / elides the allocation and the `putfield` — the field stays `null` and the
method returns `null`. This is the **kafka bug-25 escape-analysis family**
(`docs/kafka-suite-bugs/bug-25-*` / memory `reference_kafka_suite_bugs_09_12`): the catch-all
arm of the escape pass forgetting operand provenance across un-modeled opcodes, leaving an
escaping object mis-classified as non-escaping. Bug-25's fix covered primitive-load/const/
getstatic; a store-to-instance-field-then-return path may be a sibling gap.

Alternative (less likely): a null-check-elimination or `putfield`/`areturn` ordering bug that
reads the field before the store is committed.

## Fix direction
1. Reproduce against the real class with JIT disasm:
   `CRATONVM_DBG_JIT_DISASM=1` while running `CredentialModelTest.canCreateDefaultCredentialModel`
   (or a harness that JIT-compiles `getAdditionalParameters`), and inspect whether the
   `new MultivaluedHashMap` emits an allocation + `putfield` or is elided.
2. In `analyze_escapes`, ensure a value consumed by `putfield` (store to a non-local/instance
   field) **and** by `areturn` is marked escaping (cannot be scalar-replaced). Cross-check the
   bug-25 provenance-tracking arms for the `getfield`→`new`→`putfield`→`areturn` window.
3. Verify the bare-`Box` repro stays green and that bintrees18 checksum is unchanged
   (`68332206`) — escape-analysis changes are throughput- and correctness-sensitive.

## Repro
```
cratonvm.exe --java-home <jdk> -cp "<kc-universal-cp>;<kc-runner>" \
    KcRunner org.keycloak.models.credential.CredentialModelTest     # JIT: 1 fail
cratonvm.exe --nojit ...                                            # passes 5/5
```
The kcfull harness/classpath: `apps/keycloak/kc-universal-cp.txt` + `apps/keycloak/kc-runner`.
