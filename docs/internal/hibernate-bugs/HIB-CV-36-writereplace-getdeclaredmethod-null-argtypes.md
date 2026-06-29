# HIB-CV-36 — serialization `writeReplace` mis-resolution (getDeclaredMethod null argTypes ≠ no-arg)

**Status:** RESOLVED · **Mode:** real-JDK (JIT on/off both) · HotSpot passes
**Failing test:** `org.hibernate.orm.test.serialization.TypedValueSerializationTest` (1/1)
**Baseline:** `7b2fca43` (failed) → fix branch `fix/hib-cv-36-writereplace` commit `ade1e317`

## Symptom
```
java.lang.IllegalArgumentException: Method.invoke: wrong number of arguments for
  org/mockito/internal/creation/bytebuddy/ByteBuddyCrossClassLoaderSerializationSupport.writeReplace:
  expected 1, got 0
```
Thrown from CratonVM's `Method.invoke` native at `native-builtins/src/lang_class.rs`
(arg-count vs `param_descs.len()` guard). The IAE itself was *correct* — it fired
on a method that should never have been resolved in the first place.

## Root cause (CORRECTED)
The original triage hypothesis (a Method mirror whose `parameterTypes.length`
disagreed with its descriptor) was **wrong**. `create_method_object` builds
`parameterTypes` directly from the parsed descriptor, so they always agree.

The real bug was in the resolution native. Java serialization resolves the hook via
`java.io.ObjectStreamClass.getInheritableMethod(cl, "writeReplace", null, Object.class)`,
i.e. `cl.getDeclaredMethod("writeReplace", (Class[])null)`. Per JDK semantics a
**null** `parameterTypes` array is equivalent to an **empty** one — `Class.searchMethods`
compares via `arrayContentsEq(null, p)`, which is true iff `p` is empty — so the
query must match **only a no-arg** method.

`native_class_get_declared_method` (and its twin `native_class_get_method`) wrapped
the entire arity/type comparison in `if let Some(pt_arr) = param_types_arr { … }`.
When `param_types_arr` was `None` the check was **skipped entirely** and the loop
returned the *first* same-named method regardless of arity. So the no-arg
`writeReplace` probe matched the 1-arg `writeReplace(Object)` overload (Mockito's
`ByteBuddyCrossClassLoaderSerializationSupport`); `ObjectStreamClass.invokeWriteReplace`
then called it with 0 args → IAE.

The constructor natives (`getDeclaredConstructor` / `getConstructor`) already handled
the `None` case correctly (matched no-arg) — the two method natives were the outliers.

## Fix (`ade1e317`)
In both `native_class_get_declared_method` and `native_class_get_method`: treat a
`None` `param_types_arr` as expected-count 0 (parse the descriptor, require
`param_descs.len() == 0`), instead of skipping the check. `None` and an empty array
are now semantically identical (and share the same empty LinkResolver cache key,
which is correct). Stale "ambiguous → first match" comment removed.

## Verification
- Standalone probe (machine-free): class declaring only `Object writeReplace(Object)`
  → `getDeclaredMethod("writeReplace")` throws `NoSuchMethodException` (matches HotSpot);
  the 1-arg overload still resolves and reports `parameterCount=1`.
- `TypedValueSerializationTest`: **1/1 PASS** (`ok=1 failed=0`) vs IAE on baseline `7b2fca43`.
- `lang_class` unit suite: 134/134 incl. new regression test
  `hib_cv36_get_declared_method_null_argtypes_matches_only_no_arg`.
- `InstanceIdentityTest` (adjacent entry in `refl.txt`) fails identically on baseline
  and fix → pre-existing, unrelated (persister type-identity); not a regression.

## Repro
```
cd apps/hib-suite-runner            # target/ must exist; common.args has maxParallelForks=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.batch=1 \
  CratonRunner "C:/craton/CratonVM/apps/hib-suite-runner/refl.txt" 0
```
