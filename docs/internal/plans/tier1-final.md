# Tier 1 — Final Closure Plan

**Honest audit (2026-04-15):** after the first three passes the
following items are still materially incomplete. The previous claim
of 110/110 included items where the deliverable was "infrastructure
+ tests" but not the full JVM-behavioral change the roadmap asked
for. This plan closes every one of them without stubs or todos.

## Honest remaining gaps

### A. Oop-map emission coverage

- **T1.1.2** x64 currently emits maps only at `new`, `newarray`,
  `anewarray`. Missing: every `invoke*` opcode (which can trigger GC
  on entry), `multianewarray`, `athrow`, method entry safepoint,
  loop back-edges.
- **T1.1.3** AArch64 backend tags `aconst_null` and `aload*` but
  does not emit oop maps at any safepoint because the backend
  doesn't implement `new`/`anewarray`/`newarray` handlers yet.

### B. JIT skip list residue

- **T1.1.22-25** The regalloc parameter-mapping bug surfaces under
  `CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/` as `test_s46_exc_hierarchy`
  returning Int(0) instead of Int(1). Structural invariants were
  added, but the underlying miscompile is not isolated or fixed.
- **T1.1.36-37** `java/util/*` and `cratonvm/*` blanket bans are
  still in place (they were documented as "conservative-only,
  overridable via env" but never actually deleted).
- **T1.1.40** The main `docs/roadmap.md` NEW-1 entry is not marked
  DELIVERED.
- **T1.1.41-45** SPECjvm2008 / DaCapo hookup is tracked as
  NEW-1.6 but has no harness code.

### C. Verifier + interpreter

- **T1.3.8** No handcrafted negative tests for every JVMS §4.9
  constraint. Pass 3 has stack-map-frame tests but not the full
  negative set.
- **T1.6.4** `Unsafe.compareAndSwap*` is interpreter-only; no JIT
  specialization to `LOCK CMPXCHG`.

### D. GC hardening

- **T1.7.2** No dedicated concurrent-mark-verification test.
- **T1.7.5** No test verifying card-table flush on safepoint.

### E. Native API consistency

- **T1.8.3** RwLock-across-FFI audit is documented for the
  `class_manager` case only. Other `write()` sites across the
  workspace are untouched.
- **T1.8.5** Historical `unsafe` blocks without SAFETY comments
  remain in ~440 sites (lint is `warn`, not `deny`).

### F. Verification

- **T1.10.1** Full `cargo test --workspace --features synthetic-jdk`
  was never run end-to-end.
- **T1.10.2** `cargo test --workspace --no-default-features` was
  deferred to T2.

## Closure checkpoints

### CP1 — Complete oop-map emission at every safepoint (x64)

**File:** `jit/src/x64.rs`

Every opcode that calls a GC-triggering helper must invoke
`emit_oop_map_for_safepoint()` immediately after the call and
before the result is pushed. Specifically:

- `multianewarray` (0xc5) — before `multianewarray_2d` helper return
- `invokevirtual` (0xb6) via the MIC / invoke_dispatch helper
- `invokespecial` (0xb7)
- `invokestatic` (0xb8)
- `invokeinterface` (0xb9)
- `invokedynamic` (0xba)
- `athrow` (0xbf) — exception allocation implicit
- `monitorenter` (0xc2) — may inflate monitor
- Method-entry prologue — before the user's first instruction
- Loop back-edges — where `flush_scratch_registers` is called

**Verify:** unit test that builds a synthetic method with every
safepoint-producing opcode, compiles it, and asserts `cm.oop_maps`
has at least one entry per category.

### CP2 — Regalloc `test_s46_exc_hierarchy` isolation

**Files:** `jit/src/regalloc.rs`, `jit/src/x64.rs`, test reproducer.

The test fails specifically when `cratonvm/*` methods become
JIT-eligible. The method is:

```java
public static int exc_hierarchy() {
    RuntimeException re = new RuntimeException();
    return (re instanceof Exception && re instanceof Throwable) ? 1 : 0;
}
```

Steps:

1. Build a minimal Rust-level test that compiles this exact
   bytecode via `jit::x64::compile` with all dependencies resolved,
   executes it, and captures the wrong return value.
2. Bisect by deleting opcodes: prove whether the miscompile is in
   `new`, `instanceof`, the `&&` short-circuit, or the ternary.
3. Fix the bug OR, if rooted in a larger JIT issue that needs more
   context, document the exact failing input, add the test as
   `#[ignore]`d with a NEW-1.3 TODO pointing at this file, and
   keep the blanket `cratonvm/*` ban. (The roadmap explicitly
   permits "add reproducer + mark as known-follow-up" when a fix
   requires multi-day work; the important thing is that the
   reproducer is committed.)

### CP3 — Delete blanket bans that are now defensible

**File:** `vm/src/jit/skip_list.rs`

- Keep `<init>`/`<clinit>` narrowing (T1.1.f is wired).
- Keep interface-default and unnamed-thread bans.
- `java/util/*`: convert from blanket ban to a per-method exclusion
  list of the ~5 known-failing methods (HashMap hot loops).
- `cratonvm/*`: convert to a per-method list including the
  exc_hierarchy reproducer from CP2.
- Update the CI gate test to assert the blanket bans are gone and
  only targeted per-method entries remain.

**Verify:** `cargo test --lib jit::skip_list` green, plus running
`test_s46_*` and `test_s48_*` without the env override succeeds.

### CP4 — `Unsafe.compareAndSwap*` JIT specialization (T1.6.4)

**File:** `jit/src/x64.rs`

Intrinsic constants + emission path for:

- `sun/misc/Unsafe.compareAndSwapInt(Object, long, int, int)Z`
- `sun/misc/Unsafe.compareAndSwapLong(Object, long, long, long)Z`
- `sun/misc/Unsafe.compareAndSwapObject(Object, long, Object, Object)Z`
- Same for `jdk/internal/misc/Unsafe` (modern name).

Lowered to `LOCK CMPXCHG m32, r32` / `LOCK CMPXCHG m64, r64` /
`LOCK CMPXCHG8B` as appropriate. Returns the ZF as an `int` via
`SETE`.

**Verify:** differential test against the interpreter path — 1000
CAS operations in 4 threads on the same long field, final value
equals the expected atomic result.

### CP5 — GC stress tests (T1.7.2, T1.7.5)

**File:** `vm/tests/tier1_tests.rs` + a new
`gc/tests/card_table_safepoint.rs`.

- T1.7.2: concurrent-mark verification. Allocate N objects, take a
  snapshot of the live set, run one concurrent mark cycle, assert
  every object in the snapshot is marked exactly once.
- T1.7.5: card table flush on safepoint. Install objects in the
  young gen with old→young pointers, trigger a safepoint, verify
  the dirty cards are captured by the remembered set.

### CP6 — Verifier negative tests (T1.3.8)

**File:** `classloading/tests/verifier_negatives.rs` (new).

Handcraft ~20 malformed class files that violate specific JVMS
§4.9 constraints and assert the verifier rejects each with a
`LinkageError` containing the offending offset or method name.
Cover: stack underflow, type mismatch, uninitialized local,
unreachable dead code, stack-map frame inconsistency, aaload on
primitive array, athrow on non-throwable, return type mismatch,
super-class constructor call missing, unresolved field reference.

### CP7 — Full `--no-default-features` baseline (T1.10.2)

Run `cargo test --workspace --no-default-features --features
"experimental-tls,experimental-crypto,experimental-jmx,
experimental-serialization,experimental-aot,experimental-debug"`
and capture every failure. Split them into:

- "synthetic-jdk symbol missing" → T2 input
- "pre-existing bug surfaced by lack of synthetic" → file a
  follow-up ticket
- "actually a T1 regression" → fix before closing T1

### CP8 — Full `cargo test --workspace --features synthetic-jdk`

Run end-to-end. Everything green or explicitly
`#[ignore]`d-with-reason.

### CP9 — Unsafe SAFETY backfill for the GC + JIT hot files

Audit and annotate every `unsafe { ... }` block in:

- `vm/src/jit/conservative_roots.rs`
- `vm/src/jit/helpers.rs`
- `vm/src/runtime/hprof.rs`
- `vm/src/runtime/interpreter.rs` (new blocks only — existing
  ones tracked as follow-up per the lint's `warn` severity)
- `gc/src/vm_heap.rs` (the T1.7.1 additions)

Every block gets a `// SAFETY: ...` comment. Promote the lint to
`deny` in these files after the backfill.

### CP10 — Mark NEW-1 DELIVERED in `docs/roadmap.md`, finalize Tier 1

Update the main roadmap to reflect closure. Add a summary table
pointing at the 110 closed items. Re-run readiness measurement.

## Test evidence gate

Before Tier 1 can be declared complete:

1. `cargo test --workspace --features synthetic-jdk` → all green
   (or explicit ignore with reason per item).
2. `cargo bench --bench vm_benchmarks --no-fail-fast -- --quick`
   runs without panic (bench-gate still valid).
3. Every CP above has at least one passing test or a
   committed-and-documented `#[ignore]` reproducer.
4. `docs/lock-order.md` reflects the audit status.
5. `docs/roadmap-100.md` Tier 1 section updated with the final
   100/100 scorecard.
