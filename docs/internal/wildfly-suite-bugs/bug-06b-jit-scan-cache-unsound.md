# Bug 06b — ReflRepro GC corruption is a register-resident missed root (precise-maps gap) — **OPEN**

**Severity:** High — **CratonVM-only**, JIT-on only. The non-moving young sweep
reclaims a *live* reflection-result object whose only reference, at sweep time,
lives in a JIT **register** (never spilled to the stack). The slot is reused as a
bare `java/lang/Object`; the result is either a crash (sweep-walk desync,
`implausible object size`, rc=132/139) or — once the slot is over-scanned by a
wider conservative pass — a silent wrong result (`MISMATCH: java.lang.Object@…`,
~1 in 8000 reflective calls under GC stress).

**Status: OPEN.** This is the residual tracked after
[bug-06](bug-06-jit-junit-discovery-reflection-corruption.md) (reflection mirror
arrays not pinned). It is **NOT** the JIT-scan cache and is **NOT** fixable by a
conservative-scan tweak — see "What does NOT fix it" below. The real fix is
precise oop maps for JIT frames / a complete shadow stack (the deferred Stage
B/C work).

## Reproduce

```
CRATONVM_DBG_GC_STRESS=65536 cratonvm.exe --java-home "<jdk25>" \
  -cp wildfly-suite/repro  ReflRepro 8000        # rc=132/139
```

`ReflRepro.scan` (JIT-compiled) calls `Class.getDeclaredFields/getDeclaredMethods`
(natives that allocate) and concatenates with `StringBuilder` — i.e. a JIT method
making native calls whose object results land in JIT registers.

## Root cause (corrected)

`SWEEP_EDGES` reports, on every corrupting sweep, `root=0 young-survivor=0
old-gen=0` — the reclaimed node has **no** heap edge, no passed root, no
old→young card. Per the in-tree detector comment, that means *"the only live
reference is a register/native-stack root the sweep cannot see."* The decisive
toggle matrix on current dev (`df304353`):

| Config | Result |
|---|---|
| default | crash |
| `NO_JIT_SCAN_CACHE` (cache off) | crash |
| `JIT_SCAN_CACHE` (cache on) | crash |
| `DBG_FULLSTACK_SCAN` (whole native stack as roots) | **no crash, but `bad=1`** (`java.lang.Object` mismatch persists) |
| `DBG_FORCE_MOVING` (moving collector) | no crash |
| `DISABLE_JIT` | clean |

`getDeclaredFields()` etc. are dispatched from a **JIT** frame; their object
result returns in a register (`rax`). Between the native return and the JIT
spilling/storing it, the object's only reference is that register. The non-moving
young sweep (forced because a JIT frame is live — `gc_quiescence`) marks from the
conservative root set, which scans the *stack* only; a register-resident oop is
invisible. The sweep reclaims the object → the slot is reused as a bare
`java/lang/Object` (the bug-06 signature). `FULLSTACK_SCAN` scanning the whole
stack catches the cases where the value *has* spilled, dropping the crash rate to
the rare still-in-register window — hence `bad=1`, not 0.

## What does NOT fix it (ruled out)

- **The JIT-scan cache** (`JIT_SCAN_CACHE`). An earlier pass mis-attributed the
  crash to the cache being unsound and disabled it by default. On the original
  base (`0e3f0398`) disabling it *appeared* to fix the crash — but that was only
  a GC-timing perturbation: on current dev the crash is identical cache-on and
  cache-off. The cache default-off was reverted (it was a perf regression for no
  benefit). The cache *does* have a real, separate weakness — reusing a snapshot
  across a GC could republish a freed address — which is now closed by keying it
  on `heap.collection_count()` (`JitScanCache::collection_count`); that hardening
  was kept.
- **A full native-stack conservative scan** (`DBG_FULLSTACK_SCAN`). Reduces but
  does NOT eliminate the corruption (`bad=1`), because the missed root is in a
  register, not on the stack.

## Real fix (required, not yet implemented)

Precise oop maps for JIT frames (so a register-resident oop at a safepoint is
exactly described and either spilled or relocated), or a **complete** shadow
stack (`CRATONVM_SHADOW_STACK` spills register oops before GC-capable calls —
it does not crash here, consistent with the diagnosis, but is incomplete/slow).
Both are the tracked Stage B/C "precise JIT maps" work. A narrower interim is to
have JIT codegen spill the live oop set (or at least native-call return values)
to stack spill slots before every GC-capable call from a JIT frame, so the
existing conservative stack scan can see them.
