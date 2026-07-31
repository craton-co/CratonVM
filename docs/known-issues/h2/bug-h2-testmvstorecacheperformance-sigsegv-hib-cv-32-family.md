# `TestMVStoreCachePerformance` crashes with `SIGSEGV` after a burst of heap-integrity defensive-guard warnings — HIB-CV-32-family, guards don't fully prevent the crash here

## Status
**OPEN** — a hard `SIGSEGV` preceded by a burst of
`gen_heap::set_field: out-of-bounds field write dropped ... class_id=ClassId(0)
class_name=java/lang/Object num_slots=0` guard warnings. Found while re-running
the H2 suite's HANG classes with a longer (1500s) per-class timeout.

> **Correction (2026-07-31).** The original text of this section cited
> `TestGetGeneratedKeys` as a milder sibling occurrence of the same
> `HIB-CV-32` heap-corruption family. **That corroboration is withdrawn** —
> `TestGetGeneratedKeys` was root-caused and fixed and had *nothing* to do
> with heap corruption: its `gen_heap::read_slot: corrupt Value cell`
> diagnostics came from `Integer/Boolean/...equals(Object)` natives reading
> field 0 of an *array* argument, because they never performed the JDK's
> `instanceof` test. See
> `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testgetgeneratedkeys-wrapper-equals-missing-type-check-FIXED.md`.
> The `HIB-CV-32` attribution for *this* crash is therefore un-corroborated
> and still unverified — treat it as a hypothesis, and confirm the holder's
> `kind=`/backtrace with `CRATONVM_DBG_CELLCORRUPT=1` before assuming a
> GC/reference-integrity cause.

**Faster reproducer available (2026-07-31):** this crash's exact signature
(the `class_id=ClassId(0) java/lang/Object num_slots=0` write-guard burst,
then `SIGSEGV`) also occurs in `org.h2.test.synth.TestDiskFull`, which takes
**~2 s** per attempt instead of `TestMVStoreCachePerformance`'s ~972 s. See
`bug-h2-testdiskfull-classid0-corruption-segv-cce.md`.

## Severity
**HIGH** — a hard `SIGSEGV`, not a catchable exception or a clean test
failure.

## Affected test class
`org.h2.test.store.TestMVStoreCachePerformance`

## Symptom
Immediately before the crash, a burst of two distinct defensive-guard
warnings fires repeatedly:
```
[WARN] gen_heap::set_field: out-of-bounds field write dropped (caller used
  slot index past receiver's layout — class layout is correct; the bug is
  in the caller's slot computation) obj=0x200274cf3a8 index=0 num_slots=0
  class_id=ClassId(0) class_name=java/lang/Object real_field_count=Some(0)
  value=Object(Some(ObjectRef { ptr: 0x20026751978 }))
  ... (repeats for at least 10 distinct objects, all class_id=ClassId(0)/
      java.lang.Object, all num_slots=0, all index=0)

[ERROR] gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)
  — returning null instead of a UB-on-match Value. Heap reference-integrity
  defect (see HIB-CV-32). slot=0x20010419010 raw0="0x0000003436363834" raw1="0x0102080100000000"
  ... (fires 3x, two distinct slots)

#
# A fatal error has been detected by the CratonVM Runtime Environment:
#  SIGSEGV at pc=0x626dfd6bf929, addr=0x0, pid=1820707, ...
```

## Analysis
The `set_field` warnings are a **different** guard than the `read_slot`
one already tied to `HIB-CV-32`, but describe the same underlying shape of
defect: a live object reference (`ObjectRef { ptr: ... }`) is about to be
written into slot 0 of a receiver whose header claims `class_id=0`
(`java/lang/Object`) with **zero fields** — i.e. the receiver's own header
looks like a bare `Object`, not whatever real, field-bearing class it
should be. This is architecturally the same signature as `HIB-CV-31`'s
already-documented finding (`recv_cid=0 recv_class=java/lang/Object` on a
reference that should have pointed at a concrete object) and this session's
own `TestGetGeneratedKeys` finding (`gen_heap::read_slot` catching a
corrupt cell) — all three read back a **plausible-looking but wrong**
`java/lang/Object`/`ClassId(0)` shape where a real, concrete object should
be.

The crucial difference here: **10+ occurrences of the write-side guard and
3 occurrences of the read-side guard all fired and were safely absorbed**
(dropped/nulled, no crash from any of them individually) — but the process
still `SIGSEGV`'d moments later. This means either:
1. The corruption is wide enough under this specific workload's allocation
   pattern that *some* corrupted access isn't covered by either existing
   guard (a getfield/putfield path, an array access, or a JIT-compiled
   fast path that bypasses `gen_heap::{read_slot,set_field}` entirely —
   `--nojit` was NOT confirmed for this specific run, worth checking
   first), or
2. One of the "successfully" dropped/nulled writes or reads itself leaves
   the object graph in a state that a *later*, ordinary (non-corrupt-looking)
   access then dereferences incorrectly (e.g. a `null` substituted for what
   should have been a real reference, then unconditionally dereferenced
   without a null-check a few instructions later, in either interpreted or
   JIT-compiled code).

Not root-caused further this session — this is a "found a crash, connected
it to the closest known defect family, and stopped" flag, not a
root-caused fix write-up.

## Suggested next steps
1. Re-run with `--nojit` explicitly (confirm whether this is JIT-only,
   interpreter-only, or both) — none of the three related HIB-CV docs
   (`HIB-CV-31`/`-32`/this session's `TestGetGeneratedKeys` finding) have
   directly confirmed this specific test's JIT-vs-interpreter sensitivity.
2. Capture the actual `hs_err_pid*.log` this run produced (referenced in
   the crash banner but not retrieved this session) for the faulting
   frame's Java-level context — the raw crash banner alone doesn't show
   which bytecode/JIT frame dereferenced the null pointer at `addr=0x0`.
3. Cross-reference against
   `docs/internal/fixed-suite-bugs/hibernate/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md`'s
   own root-cause mechanism (`gen_heap.rs`'s `promotion_oom_risk` diverting
   `--nojit` young collections into a corrupting non-moving sweep) to see
   whether this workload's own allocation/GC pattern (cache eviction under
   sustained throughput — `TestMVStoreCachePerformance` is explicitly a
   cache-churn benchmark) hits the same trigger condition, or a distinct
   one that the June 23 fix doesn't cover.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.store.TestMVStoreCachePerformance
```
Reproduced once this session (~972s to crash, JIT enabled per the suite
runner's default for this pass — `--nojit` not yet tried); not yet
confirmed deterministic across repeated runs.

## Related
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testgetgeneratedkeys-wrapper-equals-missing-type-check-FIXED.md`
  — FIXED, and **not** this family (see the Status correction above).
- `bug-h2-testdiskfull-classid0-corruption-segv-cce.md` — same guard-burst +
  `SIGSEGV` signature, ~2 s per attempt.
- `docs/internal/fixed-suite-bugs/hibernate/run-20260622/HIB-CV-31-abstractmethoderror-onflush-root-cause.md`
  and
  `docs/internal/fixed-suite-bugs/hibernate/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md`
  — the original investigation and partial fix for this defect family.
