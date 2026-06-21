# JIT miscompile — lazy-init getter returns `null` (CredentialModelTest)

**Status:** 🔴 **OPEN** — CratonVM-only, **JIT-only** (passes `--nojit`). Handoff from the keycloak
full-suite run (kcfull-2026-06-18 report 18, "Root C"). **The original escape-analysis hypothesis is
REFUTED (2026-06-21)** — see "Investigation 2026-06-21" below. Not standalone-reproducible; needs the
real keycloak class + its Jackson-deserialization caller context.
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

## Original hypothesis (now refuted — kept for the trail)
The `new MultivaluedHashMap()` escapes via `putfield additionalParameters` (store to an
instance field) **and** `areturn`. The theory was that JIT escape-analysis scalar-replaces /
elides the allocation + `putfield`, so the field stays `null`. See the refutation below.

Alternative (still open): a null-check-elimination or `putfield`/`areturn` ordering bug in the
**single-pass** backend that reads the field before the store is committed, **specific to the
register/escape state produced by the real Jackson-deser caller chain** (not the bare shape).

## Investigation 2026-06-21 — escape-analysis hypothesis REFUTED

Traced the actual JIT scalar-replacement path (`CRATONVM_JIT_SCALAR_NEW`, default-ON):
`jit/src/escape_analysis.rs` + the IR bridge in `jit/src/lib.rs` + the elision admission in
`vm/src/runtime/interpreter.rs::is_elidable_construction`.

1. **`new MultivaluedHashMap<>()` is NOT an elidable construction.** Scalar replacement of a
   `new` only happens when the constructor is *elidable*, and `is_elidable_construction` admits
   **only a direct `java/lang/Object` subclass** whose `<init>()V` is exactly the canonical
   5-byte `aload_0; invokespecial Object.<init>()V; return`. `MultivaluedHashMap` extends
   `HashMap`, so its `invokespecial <init>` is **never** added to `trivial_init_pcs`; the IR
   builder bails the method to the single-pass backend (`jit/src/ir.rs` opcode `0xb7`). **The
   getter never reaches the escape-analysis / scalar-new path at all.** This is also why the
   bug-doc's bare `new Object()` `Box` repro never reproduced — `Object` is likewise not an
   elidable subclass-of-Object.
2. **The bare lazy-init shape compiles correctly on BOTH backends.** Standalone repros
   (`repros/keycloak-credentialmodel-jit/`): `LazyNull` (instance getters with a `HashMap`
   subclass + a raw `HashMap`, single-pass) and `EaRepro` (a `Leaf` that *is* an elidable
   Object-subclass, IR path) both print `RESULT=OK` on the pre-fix binary under JIT, `--nojit`,
   and HotSpot — 3–5M iterations, zero null returns.
3. **Conclusion:** the failure is **not** the escape pass and **not** the bare getter shape. It
   needs the real `PasswordCredentialData`/`PasswordSecretData` classes reached through the
   Jackson deserialization call chain (exactly the caller-context dependence the Symptom section
   already flagged). Reproducing it requires the keycloak classpath (`kc-universal-cp.txt` /
   `kc-runner`), which is not in-tree.

### Side discovery + fix: escape-lattice `Op::Param` soundness hole (`jit/src/escape_analysis.rs`)
While auditing the escape pass, found a real (if currently latent) bug: `Op::Param` nodes were
never assigned an escape state, so they defaulted to `NoEscape`. The store-publish rule's own
comment claims to "cover Param/Call holders", but with `this`/argument holders defaulting to
`NoEscape` the rule never fired — a value published into `this.field` was not forced to escape,
and `find_lock_elisions` (which gates purely on escape state) would wrongly elide a lock on a
parameter. **Fixed:** parameters now initialise to `GlobalEscape` (a parameter reference is, by
definition, reachable by the caller). Regression test `test_value_stored_into_param_field_escapes`.
This is **behaviour-neutral for current production consumers** (scalar replacement is independently
gated by the use-walk's `is_value`/holder-role check, and the IR builder never emits monitor nodes
so lock elision never runs on real bytecode), so it does **not** explain keycloak — it is defensive
hardening of the lattice (closes the violated invariant) carried alongside the investigation.

## Fix direction (revised)
1. Reproduce against the **real** class with the Jackson-deser caller chain (needs the kc
   classpath), with `CRATONVM_DBG_JIT_DISASM=1` on the **single-pass** compile of
   `getAdditionalParameters` / `getPasswordCredentialData`. The escape/scalar path is ruled out;
   look at the single-pass `getfield`→null-branch→`putfield`→`getfield`→`areturn` codegen and
   the register state under the deser allocation churn (could be a Family-A GC-root reclaim of
   the freshly-stored field value, not a codegen bug — re-check under `--nojit` + GC stress).
2. Keep the bare-`Box`/`LazyNull`/`EaRepro` repros green and `bintrees18` checksum unchanged
   (`68332206`).

## Repro
```
cratonvm.exe --java-home <jdk> -cp "<kc-universal-cp>;<kc-runner>" \
    KcRunner org.keycloak.models.credential.CredentialModelTest     # JIT: 1 fail
cratonvm.exe --nojit ...                                            # passes 5/5
```
The kcfull harness/classpath: `apps/keycloak/kc-universal-cp.txt` + `apps/keycloak/kc-runner`.
