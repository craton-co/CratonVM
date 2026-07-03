# JIT inline-alloc header corruption under Hibernate batch workloads (OOM) — OPEN

**Status:** OPEN — root cause not isolated, only characterized.
**Discovered:** 2026-07-03, while chasing HIB-CV-38 (see
[`docs/internal/hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md`](../internal/hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md)
for the unrelated bug that doc was originally filed for — that one is fixed).

## Symptom

`org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest.testMultiLoad`
(2000-row batch insert, then a 2000-id multi-load) run with JIT on, real-JDK,
`-Xmx 1500m`, via `apps/hib-suite-runner`'s `CratonRunner` (dev `62f1f39a` +
the HIB-CV-38 Boolean fix):

```
GC: inconsistent header — kind=Object but array_length=1 (num_slots=1715269, class_id=0);
  inline-alloc forgot to set kind=Array. Treating as corrupt so the walker can re-sync.
GC: skipping suspected false root at 0x27e97018 (...)
non-moving sweep: no free-block anchor remains after offset 4010576 — abandoning rest of arena (389205424 bytes retained)
java.lang.OutOfMemoryError: Java heap space
```

5/6 soak runs (JIT on) hit this and OOM after 350-550 seconds. The 6th got
further and hit an unrelated `ConstraintViolationException` (likely stale H2
state from a previous run's incomplete `@AfterEach` truncate after an earlier
crash, not itself evidence of a distinct bug).

With `--nojit`, the corruption/OOM does not occur: `testDynamicBatchFetch`
passes (`ok=1`) and `testMultiLoad` instead hits a plain 120s JUnit
`@Timeout` (interpreter is simply slow for a 2000-iteration persist loop) —
no GC warnings, no corruption. This confirms the corruption is JIT-inline-alloc
specific, not present in the interpreter path.

## Why this is a new/open issue, not a reopening

The warning text (`kind=Object but array_length=N ...; inline-alloc forgot to
set kind=Array`) is the exact signature documented and marked **FIXED** in
`docs/internal/app-jvm-bugs/jit-bintrees18-inline-alloc-and-bc-ec-round2.md`
(BUG 1): a publish-before-initialize race in `emit_inline_tlab_new`
(`jit/src/x64.rs`), where the TLAB cursor commit was ordered before the
object header was fully written, letting a concurrent heap walk observe a
TLAB-zeroed header. That fix (write the full header before the cursor commit)
**is present on current dev** — confirmed by reading `emit_inline_tlab_new`
directly — and `bintrees18` itself is checksummed correct
(`docs/internal/gaps/gap-bintrees18-gc-throughput.md`).

So this is a **different trigger of the same corruption family**, not a
regression of the bintrees18 fix. Candidates not yet distinguished:

1. A separate inline-allocation fast path (e.g. for arrays specifically —
   `anewarray`/`newarray`, or the `compact_ref_fields` branch inside
   `emit_inline_tlab_new` itself) that has an analogous
   publish-before-initialize ordering bug not covered by the bintrees18 fix
   (which was analyzed against plain 2-field `Node` objects, never arrays).
2. bintrees18 allocates one uniform object shape at high frequency; Hibernate's
   batch path allocates a heavy *mix* of shapes/sizes (Strings, JDBC parameter
   arrays, HashMap/collection backing arrays, entity instances) — possibly
   exposing a race that needs that heterogeneity (e.g. TLAB-boundary or
   GC-timing interaction the uniform-shape bintrees18 workload doesn't hit).
3. `class_id=0` in most of the observed warnings is consistent with the
   walker genuinely reading a TLAB-zeroed header (the exact pre-fix
   bintrees18 signature) — suggesting the SAME race, just via a code path the
   prior fix didn't reach, rather than a new mechanism.

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.trace=1 -Dcraton.batch=1 CratonRunner "lst_cv37_batch.txt" 0
```

(`lst_cv37_batch.txt` contains just `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`.)
Needs a binary built with the HIB-CV-38 Boolean fix — without it, this run
fails earlier at JUnit-launcher bootstrap before ever reaching `testMultiLoad`.

## Next steps

- Check whether array allocation (`anewarray`/`newarray`) has its own inline
  TLAB fast path distinct from `emit_inline_tlab_new`, or whether it always
  falls through to a helper — if the latter, look at the `compact_ref_fields`
  branch inside `emit_inline_tlab_new` for the same header-vs-commit ordering
  bug bintrees18 had.
- A larger `-Xmx` (per the HIB-CV-37 truth table pattern, where a big heap
  suppressed a similar symptom) would help distinguish "genuine leak from
  abandoned arena regions" vs. "this workload's live set is just larger than
  1500m" — not yet tried.
- `CRATONVM_JIT_DISABLE_INLINE_NEW=1` (the exact toggle that isolated BUG 1 in
  the bintrees18 doc) on this repro: tried on the sibling
  `family-a-throttle-park/MiniThrottle.java` repro (same corruption
  signature) — does NOT eliminate the corruption there (3 runs, 95-134
  warnings each). Array allocation was also ruled out structurally: `newarray`/
  `anewarray` have no inline TLAB fast path in `jit/src/x64.rs` at all — they
  always call `jit_newarray`/`jit_anewarray_object`, which build the header via
  the same non-inline, already-safe `heap.try_alloc_array` the interpreter uses.

## 2026-07-03/04 — two real conservative-root-scan bugs found and fixed (session on `fix/family-a-inline-alloc-header`); corruption family only partially closed

Deep investigation via a new forensic breadcrumb (`CRATONVM_DBG_A2`, extended
this session to also record JIT inline-`new` and TLAB-tail-filler header
writes — see `gc/src/a2dbg.rs`) on the `MiniThrottle` sibling repro found and
fixed **two real, previously-undiscovered bugs** in the conservative-root
mark path, but did **not** fully close this corruption family — it remains
OPEN, now much better characterized.

### Bug 1 (FIXED): `is_object_address` missing the extent-vs-arena-bound check

`GenerationalHeap::is_object_address` (`gc/src/gen_heap.rs`, the canonical
validator used by ~28 call sites across the VM: root scanning, cross-thread
takeover, selective-promotion pinning, etc.) validated a candidate's
`kind`/`num_slots`/`array_length` bounds but never checked that the object's
full CLAIMED EXTENT (`HEADER_SIZE + body`) actually fit inside the arena the
candidate's address falls in. `sweep_young_non_moving`'s own `mark_young`
closure already had this exact hardening (added 2026-07-03 for the DoHead
comb-7 SIGSEGV) — `is_object_address` did not. Fixed by adding the same
extent-vs-`region_bounds` check, centrally, so every caller benefits.

### Bug 2 (FIXED): `mark_young` wrote `GC_FLAG_MARKED` through unverified non-zero-word0 candidates

The real corruption mechanism, root-caused via the breadcrumb: a conservative
root candidate that merely LOOKS like a plausible header (passes
kind/num_slots/extent checks) is not necessarily the true start address of a
real allocation. A `mark_young` comment already documented the **zero-word0**
case of this hazard (a candidate at `live_object_start - 8`, mark-write at
`candidate+21` landing at `victim+13` — flipping bit 9 of the victim's real
`array_length`, producing exactly the "array_length=512" signature) and
routed zero-word0 candidates through a write-free `side_marks` set instead of
writing through them. But any candidate whose first header word was
**non-zero** (e.g. landing on ANY element of a live `Object[]`, where every
`Value::Object` cell's discriminant word is `VTAG_OBJECT = 4` — decoding as
`class_id=4`, and the SAME pattern 16 bytes later decoding as `num_slots=4`)
still fell through to the unconditional `header.gc_flags |= GC_FLAG_MARKED`
write. Fixed by routing **every** conservative candidate through `side_marks`
(pure retention, never write through) — matching the collector's own
documented invariant ("a conservative false-positive root only over-retains,
it can never corrupt non-pointer data"). A regression this surfaced
(`non_moving_sweep_records_identity_map_for_watched_survivor` — the
WeakHashMap-referent identity-map insert was only wired to the
header-marked branch) was fixed alongside it. `cratonvm-gc` test suite:
761/761 pass after both fixes.

### Residual: BOTH fixes verified correct (zero regressions, real Spring
### `ConcurrencyThrottleInterceptorTests` passes clean) but do NOT
### eliminate the corruption on the harsher `MiniThrottle` repro

Post-fix `MiniThrottle` distribution (5 runs): 84-276 "inconsistent header"
warnings per run, SAME `class_id=4, num_slots=4` signature, still failing to
complete within budget. Critically, `CRATONVM_DBG_A2`-instrumented runs show
the exact same false-positive address (e.g. `0x17b0c6002a0`) recurring
**identically across 8 separate GC cycles spanning tens of seconds**, with
byte-for-byte identical surrounding context each time. A moving/reused TLAB
slot would not reproduce identical content run after run — this is a
**long-lived, structurally fixed region being repeatedly fed as a
conservative root candidate**, which points at a specific CPU register (or a
fixed stack slot) of a HOT, frequently-re-entered JIT-compiled method
(plausibly the reflection/proxy `invoke` dispatch path, given both this repro
and MiniThrottle are `Method.invoke`/proxy-heavy) consistently holding an
`Object[]`-interior pointer across many safepoints/GC pauses. `mark_young`'s
extent check and side-marking correctly PROTECT the victim from corruption
(both fixes verified this), but the walker still logs "inconsistent header"
because the address genuinely does have that byte pattern on disk at every
scan — i.e. the log itself is not evidence of memory corruption post-fix, but
the underlying "why does a register/slot hold a stable interior pointer
across dozens of GCs" question is unanswered and is the recommended next
thread: instrument `scan_context`/`scan_one_frame`
(`vm/src/jit/xt_root_scan.rs`, `vm/src/jit/conservative_roots.rs`) to log
WHICH register or stack offset produces this exact candidate address, then
find the JIT-emitted code (likely in the reflection/`MethodHandle`/proxy
`invoke` compiled path) that keeps an array-interior pointer live there
instead of the array's base address.

An A/B during this investigation also found `CRATONVM_XT_JIT_ROOT_SCAN=0`
(disabling the cross-thread OS-suspend conservative takeover entirely)
dropped MiniThrottle's corruption to 0/0/0 across 6 runs — the strongest
single lever found — but a matching in-code change (delaying takeover behind
a cooperative-wait window) did NOT reproduce that improvement (avg
corruption ~52/run at a 20ms wait, no better than baseline), so the true
lever is more specific than "takeover happens at all" — most likely the
SAME static-register-candidate mechanism above, just fed at much higher
volume by `xt_root_scan`'s full-register+full-stack scan of every other
live thread on every GC. Disabling `xt_root_scan` outright was not shipped
as a fix: it is a real, load-bearing mechanism for a different family of
bugs (BUG-03/Fork6/A4) and disabling it broadly is not a safe trade without
further isolation.
