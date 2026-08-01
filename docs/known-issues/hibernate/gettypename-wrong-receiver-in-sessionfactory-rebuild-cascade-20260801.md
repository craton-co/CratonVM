# `Type.getTypeName()` dispatches on `java/lang/Integer` during a SessionFactory rebuild cascade

**Status:** OPEN — signature and correlations established, **not reproduced**, cause
not located. Filed so the signature is searchable and so the next person does not
repeat the eliminations below.

**Witness:** `org.hibernate.orm.test.hql.ASTParserLoadingTest`, JIT, 2 runs of 24
(2026-07-31, `cratonvm-hqlordinal-fix2-20260731.exe`). Both of those runs were
killed at the harness's 1200s cap (`rc=124`) without producing a result.

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/Integer.getTypeName()Ljava/lang/String;"
  caller="org/hibernate/type/descriptor/java/spi/JavaTypeRegistry.addBaselineDescriptor(Lorg/hibernate/type/descriptor/java/JavaType;)V @pc=27"
```

The caller's bytecode is

```
 1: invokeinterface JavaType.getJavaType:()Ljava/lang/reflect/Type;   -> local 2
24: invokevirtual   addBaselineDescriptor:(Type;JavaType;)V
      (inlined; its body does registry.put(describedJavaType.getTypeName(), d))
27: return
```

so the reported `@pc=27` is the pc *past* the `invokevirtual` at 24, and the
failing call is the inlined callee's `invokeinterface Type.getTypeName()` on
local 2. Local 2 is a `Class` mirror. A mirror is a `java.lang.Class` — its heap
header carries `java/lang/Class`'s id (`get_or_create_class_mirror` allocates it
with `alloc_object(class_class_id, ..)`) — so resolving the call against
`java/lang/Integer`, the class the mirror *describes*, is the defect.

**The VM recovers.** Nothing propagates to Java: no test fails because of this,
and the registry ends up correctly populated. It is visible only as this warning.

## What correlates, exactly

Per run of the affected round:

| run | SessionFactory bootstraps | `getTypeName` warnings | outcome |
| --- | --- | --- | --- |
| 1-0 | **21** | **32** | killed at 1200s (`rc=124`) |
| 1-2 | **21** | **32** | killed at 1200s (`rc=124`) |
| 1-1, 1-3, 1-4, 1-5 | 4 | 0 | 105/106 (`testJpaTypeOperator` timeout) |

Clean runs of the same binary do 3 bootstraps and emit nothing. The correlation
with the bootstrap count is exact across every run examined.

The affected runs enter a **pure bootstrap cascade**: from the first warning to
the kill, the log contains nothing but repeated SessionFactory builds — JAXB
parsing, `CachingRegionFactory`, `Database info` — with **no SQL and no test
output at all**, one cycle every ~26 seconds, two warnings per cycle. The
plausible mundane reading is that a prior 120s JUnit timeout under load left
every remaining test rebuilding the factory, at ~26s each, until the run ran out
of wall clock. That explains the *kill*. It does not explain the wrong receiver,
which is a VM-level defect either way.

The first two or three bootstraps of an affected run are clean; the warnings
begin only once the cascade starts. Something accumulates.

## Ruled out

Everything here was measured, not argued.

* **`getJavaType()` miscompiled to return the wrong field.** `IntegerJavaType`
  declares `public static final Integer ZERO` alongside the inherited
  `Class<T> type`, and the failure names Integer *every* time — never String,
  never Long — so a getstatic/getfield mix-up on this one class was the best
  structural fit. `JavaTypeAccessorProbe` drives the real
  `AbstractClassJavaType.getJavaType()` through an interface-typed site over
  nine descriptor singletons: **810 000 checks clean** across default,
  `CRATONVM_JIT_THRESHOLD=5`, `CRATONVM_GC_STRESS=1M`, and both together.
* **`Type.getTypeName()` dispatch over many `Class` mirrors.**
  `TypeNameDispatchProbe` drives one shared, hot, `Type`-typed site over 30
  mirrors (wrappers, primitives, arrays, generics): **1 800 000 checks clean**
  under forced JIT and GC stress.
* **`VIRTUAL_TARGET_CACHE` serving a stale entry after `JitInvokeInfo` address
  recycling.** The key is `(info pointer, receiver ClassId)` and the memo is
  flushed on class-identity changes but *not* on JIT generation changes, so a
  recycled `info` address looked like a live hazard. It is not one here: entries
  are only inserted when `cacheable_receiver`, and in that branch the cached
  `class_name` is the receiver class's own name — a pure function of the
  `ClassId` half of the key. A recycled `info` pointer cannot change the answer.
  (Recycled *ClassIds* could, but `java/lang/Class` and `java/lang/Integer` are
  bootstrap classes that never unload.)
* **Reproduction on current `dev`.** 16 runs of the witness class at `--par 8`
  with `CRATONVM_DBG_CCE_BT=1`: 0 warnings, and every run did 3 bootstraps —
  the cascade itself did not reproduce. 25 sequential bootstraps
  (`SessionFactoryChurnProbe`) with the tracer on: 0. The same at
  `CRATONVM_JIT_THRESHOLD=5` and `=25`: 0. An induced cascade
  (`-Djunit.jupiter.execution.timeout.default=15s`, 4 tests timed out): 5
  bootstraps, 0 warnings.

## How to hunt it

The blocker is producing the cascade, not observing the defect. Get a run to 20+
SessionFactory bootstraps and the warnings appear to follow deterministically;
every attempt above topped out at 5.

`CRATONVM_DBG_CCE_BT=1` is the right tracer — since 2026-07-31 it dumps the frame
stack, the receiver address, a `gcpart` probe and push provenance for **every**
dispatch `NoSuchMethodError`, not only those whose receiver resolved to bare
`java/lang/Object`. No captured occurrence has that dump yet; the two field runs
predate the flag being set. One occurrence with it on should settle whether the
receiver is a mirror whose class resolved wrongly or a different object
altogether.

Note the two field runs were on `cratonvm-hqlordinal-fix2-20260731.exe` and the
eliminations above are on current `dev`, which has since taken twenty-odd
commits including the JIT-ban removal and virtual-call devirtualization. A
`git bisect` against the fix2 binary is viable if the cascade can be induced.

## Probes

Tracked next to the suite runner; all three are HotSpot-clean.

| Probe | What it drives |
| --- | --- |
| `JavaTypeAccessorProbe.java` | `JavaType.getJavaType()` through an interface-typed site over nine descriptor singletons, result used as a map key via `getTypeName()` |
| `TypeNameDispatchProbe.java` | one shared `Type`-typed `getTypeName()` site over 30 distinct `Class` mirrors |
| `SessionFactoryChurnProbe.java` | repeated real SessionFactory bootstrap, for defects that surface once per build |
