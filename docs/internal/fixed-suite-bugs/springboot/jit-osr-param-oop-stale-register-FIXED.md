# HIB-CV-20 — OSR-compiled methods left reference *parameters* stale across a moving GC (FIXED)

**Status:** FIXED on branch `fix/hib-cv-20-jit-xsd-hang` (worktree `C:/craton/CratonVM-hib20`).
**Symptom class:** JIT-on hang / heap corruption; default config; reproducible.
**Original report:** `apps/hibernate-orm/cratonvm-bug-reports/run-20260622/HIB-CV-20-jit-xerces-xsd-schema-parse-hang.md`

---

## Symptom

`org.hibernate.boot.jaxb.internal.stax.LocalXmlResourceResolverTest` (and other
SessionFactory-bootstrap tests that touch `org.hibernate.boot.xsd.MappingXsdSupport`)
**hang** under the default JIT, where HotSpot passes in ~5 s. The Hibernate-free
repro `MinSeq` (parse 13 ORM XSDs via Xerces `SchemaFactory.newSchema()` in one
process) reproduces it: it hangs around the 5th–6th parse with JIT on, completes
13/13 with `--nojit`.

The hang **site moves** between runs (`SchemaDOM.characters`, `XSDFACM.buildDFA →
CMStateSet.equals`, `main`, …) and one run's watchdog reported **7597 threads** and
dumped `main` twice — the signature of **heap corruption**, not a deterministic
codegen infinite loop.

## Root cause

Only one method ever JIT-compiles during the repro: `java/util/Arrays.fill([BIIB)V`,
via **back-edge OSR** (its internal fill loop goes hot within a single call).
Skipping just that compile (`CRATONVM_JIT_BISECT_SKIP=java/util/Arrays.fill`) makes
the whole 13-parse sequence pass — pinning the culprit to the OSR-compiled fill.

`Arrays.fill([BIIB)V` is `fill(byte[] a, int from, int to, byte val)`. JVM local 0
(the `byte[] a`) is a **reference parameter**. The register allocator keeps it in a
**callee-saved register (r15)** across the method's pre-loop
`Arrays.rangeCheck(III)V` call. `rangeCheck` failed to compile (`dup_x`-class bail),
so it is dispatched as a **call = a GC-capable safepoint**.

CratonVM has no register-oop map (see the SAFETY INVARIANT block at the top of
`jit/src/lib.rs`). Correctness across a safepoint relies on:

1. `emit_pre_safepoint_spill` — spill every register-resident local to its canonical
   frame slot `[rbp-(idx+1)*8]` before the call (so the value is GC-visible);
2. the conservative frame sweep + `remap_active_jit_frames` — a **moving** young GC
   rewrites those slots in place to the relocated address;
3. `emit_post_safepoint_reload` — **after** the call, reload each oop register-local
   from its (now GC-updated) slot, so the callee-saved register picks up the new
   address.

Step 3 only reloads locals flagged as oops in `local_oop_masks[pc]`. That dataflow
is seeded at entry from `param_oop_mask` (reference parameters). **The OSR compile
path called the legacy `x64::compile` wrapper, which hardcodes `param_oop_mask = 0`**
(`jit/src/x64.rs`), so the array parameter was *never* marked an oop. Consequence:

- pre-call: array spilled to `[rbp-8]` ✓ (kept alive / GC-visible)
- during `rangeCheck`: a moving young GC evacuates the (young, freshly-allocated)
  byte[], `remap_active_jit_frames` updates `[rbp-8]` ✓
- post-call: **no reload** → r15 keeps the **pre-GC** address ✗
- the OSR fill loop then writes `val` through the stale r15 into freed/reused young
  memory → **heap corruption** → later hang/crash in arbitrary code.

The hot-path `jit::try_compile` was already correct: it computes `param_oop_mask`
via `compute_param_oop_mask` and calls `compile_with_param_slots`. Only the two
direct callers of the legacy wrapper were affected.

The default young GC is a moving (evacuating) collector, which is why a *static*
(old-gen, non-moving) buffer does not corrupt but a *fresh young* one does — see
`scratch/OsrFillGc2.java`.

## Fix

`jit/src/lib.rs` — make `compute_param_oop_mask` `pub` (so the VM crate can reuse
the exact same reference-parameter mask).

`vm/src/runtime/interpreter.rs` — both direct callers of the legacy `x64::compile`
wrapper now call `x64::compile_with_param_slots`, computing
`param_oop_mask = compute_param_oop_mask(descriptor, is_static)` (gated on
`precise_jit_maps_enabled()`; `0` when the gate is off):

1. `compile_osr_artifact` (the confirmed culprit; shared by the background worker
   and the foreground `CRATONVM_BG_COMPILE=0` OSR path).
2. The eager first-call compile path (the latent twin; dormant when `bg_compile` is
   on, reachable with `CRATONVM_BG_COMPILE=0` / `CRATONVM_JIT_C2_FIRST_CALL`).

The legacy slot layout (`param_jvm_slots = &[]`, `param_slot_span = 0`) is preserved
verbatim, so **gate-off codegen is byte-identical** (mask = 0).

## Validation

- `MinSeq` (13-XSD Xerces parse): hang → `@@ALLDONE` rc=0 with JIT on (default).
- `scratch/OsrFillGc2.java` (fresh young byte[] + GC pressure + hot `Arrays.fill`):
  hang → completes.
- `--nojit` and gate-off (`CRATONVM_NO_PRECISE_JIT_MAPS`) paths unchanged.
- See the worktree commit message for the full before/after matrix.

## Notes

The pre-existing SAFETY INVARIANT comment in `jit/src/lib.rs` claims the default
young GC is "non-moving" so "no post-call reload is needed". That is inaccurate for
the generational evacuating young collector; the precise-map reload (default-on) is
what actually keeps register-resident oops correct, and it must be fed the
parameter oop mask on *every* compile entry point — not just `try_compile`.
