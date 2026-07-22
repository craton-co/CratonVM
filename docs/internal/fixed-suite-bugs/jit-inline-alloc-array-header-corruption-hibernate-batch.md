# JIT inline-alloc header corruption under Hibernate batch workloads (OOM) — FIXED

**Status:** FIXED on `dev` (fix commit `74dc80b8`, merged by `45a0a858`) for
the Hibernate batch OOM / corrupt-header symptom described here. The root cause
was a shared GenerationalHeap allocation publish-before-initialize race. The
original Hibernate batch fixture was not present in this checkout for an
end-to-end rerun; validation is the focused allocation-ordering regression test
plus the full `cratonvm-gc` lib suite listed below.

**Scope note:** the later MiniThrottle / Family-A residual captured at the end
of this document is not this Hibernate allocation-publication bug. That
separate root-scan residual remains actively tracked in
[`docs/known-issues/dohead-jit-heap-corruption-register-invisibility.md`](../../known-issues/dohead-jit-heap-corruption-register-invisibility.md)
and its repro notes under
[`docs/known-issues/repros/family-a-throttle-park/`](../../known-issues/repros/family-a-throttle-park/README.md).

**Discovered:** 2026-07-03, while chasing HIB-CV-38 (see
[`docs/internal/hibernate-bugs/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md`](hibernate/HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md)
for the unrelated bug that doc was originally filed for — that one is fixed).
**Also the likely explanation for HIB-CV-39** (a `DynamicBatchFetchTest` SIGSEGV
filed before the HIB-CV-38 fix landed): see
[`docs/internal/hibernate-bugs/HIB-CV-39-dynamicbatchfetch-sigsegv-regression.md`](hibernate/HIB-CV-39-dynamicbatchfetch-sigsegv-regression.md)
for the closure reasoning — with HIB-CV-38 fixed, the identical repro no longer
SIGSEGVs and instead deterministically reproduces this bug.

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

## Why this was a distinct issue, not a reopening

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
regression of the bintrees18 fix. Candidates (see "Additional evidence" for
which of these are now ruled out):

1. ~~A separate inline-allocation fast path for arrays~~ — **RULED OUT
   2026-07-03**: `newarray`/`anewarray` never had an inline TLAB path at all;
   they unconditionally call the `jit_newarray`/`jit_anewarray_object` helpers
   (confirmed by direct code read, `jit/src/x64.rs` ~23211-23385, opcodes
   0xbc/0xbd). There is no array-specific inline fast path to be buggy.
2. bintrees18 allocates one uniform object shape at high frequency; Hibernate's
   batch path allocates a heavy *mix* of shapes/sizes (Strings, JDBC parameter
   arrays, HashMap/collection backing arrays, entity instances) — possibly
   exposing a race that needs that heterogeneity (e.g. TLAB-boundary or
   GC-timing interaction the uniform-shape bintrees18 workload doesn't hit).
   **Resolved for this Hibernate symptom by the shared allocation-helper fix
   below** — plain `new`'s inline path was ruled out too (see below), so the
   race had to live outside the JIT inline-alloc codegen itself.
3. `class_id=0` in most of the observed warnings is consistent with the
   walker genuinely reading a TLAB-zeroed header (the exact pre-fix
   bintrees18 signature) — suggesting the SAME race, just via a code path the
   prior fix didn't reach, rather than a new mechanism. **Partially narrowed**:
   the corrupted `class_id`/`num_slots`/`array_length` values vary run-to-run
   (0, 4, 2000, 786439952, ...) rather than being one fixed garbage pattern —
   consistent with reading genuinely uninitialized/stale memory rather than a
   single deterministic bad address, but the *source* of that stale read is
   no longer the plain-`new` inline path (ruled out below).

## Repro

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.trace=1 -Dcraton.batch=1 CratonRunner "lst_cv37_batch.txt" 0
```

(`lst_cv37_batch.txt` contains just `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest`.)
Needs a binary built with the HIB-CV-38 Boolean fix — without it, this run
fails earlier at JUnit-launcher bootstrap before ever reaching `testMultiLoad`.

## Additional evidence (2026-07-03, `investigate/hib-cv-39-dynamicbatchfetch-sigsegv` branch)

Rebuilt `cratonvm-cli` from dev (post HIB-CV-38 merge, includes the `-XX:+UseG1GC`
concurrent-marking fix too — not relevant here since G1 is opt-in and this repro
doesn't pass `-XX:+UseG1GC`, so the default Generational/moving-young collector
is what's under test, same as bintrees18). Three isolated single-class reruns of
`DynamicBatchFetchTest` via `apps/hib-suite-runner`, `--java-home jdk-25`,
`@common.args -Dcraton.batch=1`:

1. **Baseline (JIT on, `--Xmx 1500m`)**: reproduced exactly as documented above —
   `inconsistent header` warning storm (`class_id=786439952`, `num_slots=2`,
   `array_length=2` this run) then `OutOfMemoryError` at `ms=553385`. **No
   SIGSEGV.**
2. **`CRATONVM_JIT_DISABLE_INLINE_NEW=1` (JIT still on, `--Xmx 1500m`)**: this
   disables `emit_inline_tlab_new` entirely, forcing every plain `new` through
   the `jit_new_object` helper — the exact toggle the pre-fix triage list below
   asked for, and the same toggle that isolated the bintrees18 fix. **Did NOT
   suppress the corruption**: identical warning storm (`class_id=2000`,
   `num_slots=1180939`), `OutOfMemoryError` at `ms=499794`. This rules out
   `emit_inline_tlab_new` (both the base bump-pointer path and the
   `compact_ref_fields` branch inside it) as the source — the race survives
   with that inline path fully disabled.
3. **Large heap (`--Xmx 6000m`, JIT on, inline-`new` enabled)**: corruption
   warnings still occur (`class_id=0/4/4`, various `num_slots`/`array_length`),
   so a big heap does **not** cleanly suppress this the way it suppressed
   HIB-CV-37's SIGSEGV and the `type.temporal.*` cluster's crash. Instead the
   run limps through both test methods for ~20 minutes (`ms=1194268`) before
   failing differently: transient H2 DDL/transaction errors ("An old
   transaction with the same id is still open", "Index PRIMARY_KEY_ not
   found" — H2 here is in-process/in-memory per run, so this isn't
   cross-process stale disk state; more likely downstream fallout of the same
   header corruption confusing JDBC/schema-management bookkeeping rather than
   a distinct bug) and finally `testMultiLoad` hitting JUnit's plain 120s
   `@Timeout` despite JIT being on — i.e. a big heap changes the *terminal*
   symptom but does not stop the underlying corruption from happening.

**Net effect on the "Candidates" list above**: candidate 1 (array-specific
inline path) is definitively false — no such path exists. Candidate 2's most
obvious culprit (`emit_inline_tlab_new`'s per-object-shape codegen) is now also
ruled out by evidence 2. The corruption is real and reproducible 3/3, but its
locus is narrower than originally scoped: **not** the array codegen, **not**
the plain-`new` inline-TLAB codegen. Remaining candidates: the shared
allocation helpers themselves (`jit_new_object`/`jit_newarray`/
`jit_anewarray_object`, wherever they live in `vm`/`gc`), or the moving young
GC's copy/remap routine writing a stale/short header onto an object it
relocates (which would explain corruption appearing regardless of which JIT
codegen path allocated the object in the first place).

## Pre-fix triage notes

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

## 2026-07-04 — shared allocation-helper publish-before-initialize bug fixed

Root cause fixed in `gc/src/gen_heap.rs`: young allocation reserved bytes from
`young_from` under the arena lock, released that lock, then zeroed the span and
let callers write the object/array header later. A concurrent GC/walker could
acquire the same arena lock in that window and observe a reserved but
headerless allocation. That matches the stale/zero header warning stream and
also explains why `CRATONVM_JIT_DISABLE_INLINE_NEW=1` did not suppress the
bug: JIT helper calls for `new`, `newarray`, and `anewarray` still use the
shared heap allocation path.

The fix adds `try_alloc_young_initialized` / `alloc_young_initialized`, which
reserve, zero, and run the caller's header initializer while the `young_from`
lock is still held. The direct old-generation spill paths now also write
headers while holding the `old_gen` lock, closing the same
publish-before-initialize shape for major-GC walks.

Validation from branch `codex/hib-inlinealloc-gc-20260703-001`:

- `cargo test -p cratonvm-gc young_alloc_initializes_header_before_unlocking_arena`
  passed. The regression test blocks inside the initializer and proves the
  young arena lock is held until the header write can complete.
- `cargo test -p cratonvm-gc --lib` passed: 762 tests.
- `cargo build -p cratonvm-cli --bin cratonvm` passed, and the worktree binary
  was copied to `cratonvm-hib-inlinealloc-gc-20260703-001.exe`.
- The original `apps/hib-suite-runner` fixture and `common.args` file named in
  this note are not present in this checkout, so the Hibernate batch repro
  itself was not rerun here.

## 2026-07-03/04 — related Family-A follow-up, separately tracked

Deep investigation via a new forensic breadcrumb (`CRATONVM_DBG_A2`, extended
this session to also record JIT inline-`new` and TLAB-tail-filler header
writes — see `gc/src/a2dbg.rs`) on the `MiniThrottle` sibling repro found and
fixed **two real, previously-undiscovered bugs** in the conservative-root
mark path. That work did **not** fully close the broader Family-A/root-scan
residual, which is tracked outside this archived Hibernate allocation note.

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

### Separate residual: BOTH fixes verified correct (zero regressions, real
### Spring `ConcurrencyThrottleInterceptorTests` passes clean) but do NOT
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

### Other independently-explored candidates (dev, same window) — also not the source

- ~~Check whether array allocation has its own inline TLAB fast path~~ — done,
  it doesn't (confirmed independently above too).
- ~~Try `CRATONVM_JIT_DISABLE_INLINE_NEW=1`~~ — done; does not suppress
  (confirmed independently above too). Stop looking at `emit_inline_tlab_new`
  for this bug.
- ~~Try a larger `-Xmx`~~ — done; does not cleanly suppress, just delays and
  changes the terminal symptom.
- Since neither inline-alloc codegen path is the source, look at the **moving
  young GC's object-copy/relocation code** (`gc/src/gen_heap.rs`) for a
  header-write ordering or size-computation bug on the *copy* side (as
  opposed to the *allocation* side already ruled out) — e.g. does the Cheney
  copy routine write the new copy's header fields in a safe order, and does it
  use the correct size class for a copied object of mixed/heterogeneous shape?
- Try `--nojit` with the large heap too (not yet tried in combination) to
  confirm the corruption is still JIT-gated even when the terminal symptom
  changes with heap size.
- A repro that isolates `testDynamicBatchFetch`/`testMultiLoad` into separate
  single-method runs (rather than the whole class) would help tell whether the
  corruption is triggered by the first test's setup (2 rows) or specifically
  by `testMultiLoad`'s 2000-row batch.
