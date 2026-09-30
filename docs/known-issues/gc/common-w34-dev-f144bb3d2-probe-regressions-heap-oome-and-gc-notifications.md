# Three GC probes regressed on dev between `92ad6b756` and `f144bb3d2`

> **STATUS (2026-09-29, gce e2/f, item 3 only): NARROWED.** The frame holders are fixed (ZGC = HS 5/5 on e1). The Generational residual (`alAdd=1/4`, and the empty control stdout, which is an OOME escaping `NativeGrowthReclaimProbe.java:95`) is an old-gen humongous compaction refused while compiled frames are live, not a retained object: `gce-e2f-old-humongous-compaction-refused-while-compiled-frames-are-live-20260929.md`. Item 3 retires with that page, or in the judging geometry given on `gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md` (e2/f block).

> **STATUS (2026-09-29, gce e1/x): KEEP for item 3 on Generational only.** Item 2: `GcNotificationThreadProbe -Xmx256m` (new stdout, fresh HotSpot cache) = HotSpot 2/2 on each of Generational, G1 and ZGC on e1 (`*_gcnotif_1..2`, default mode; the `--compatible` arm and a third run were not run). Item 1: `--compatible FullHeapResolveProbe -Xmx64m` rc 0 on Generational and ZGC (DIFF in identity-hash values, as at d10); G1 is `gcd-d10v-...`. Item 3: ZGC passes 5/5, Generational 3/6 (details on `gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`). **Remaining:** item 3 on Generational; item 2's `--compatible` 3x for completeness.

> **STATUS (2026-09-29, gce e1/f, item 3 only): FIXED IN CODE, awaiting the probe -- the holders are named and dropped.** A census of the base (`adb9178bc`) named them: the OSR'd single-pass `main`'s local 11 (`off=96 region=java-local in_map=false`, the previous round's `char[]` / `StringBuilder`, a verifier-`top` local), and, behind it, a never-written frame-deopt register block of the callee frame at the same stack depth (`fillAndDrop ... region=deopt-saved-gpr-image`). Two new band claims in `vm/src/jit/conservative_roots.rs` drop both (default on; `CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0`, `CRATONVM_GC_DEOPT_IMAGE_RESIDUE_ROOTS=0`). Details, tests and the run are on `gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`. **Retire item 3 when** `NativeGrowthReclaimProbe -Xmx128m` prints `alCapacity=4/4 alAdd=4/4 toCharArray=4/4 sbCapacity=4/4 PROBE-OK (sink 0)` 3/3 on Generational and ZGC on Linux (Windows is not a valid host for it on this base, see that page).

> **STATUS (2026-09-29, gce e1/o, items 1-2 on Generational): FIXED IN CODE (the probe), awaiting the probe -- nothing is left open on Generational for items 1 and 2.**
> - **Item 1** (`FullHeapResolveProbe`): passes on Generational (d10 verification, `linked=6 oome=0 PROBE-OK`; the DIFF is identity-hash values only). No change. Its G1 row is `gcd-d10v-g1-heap-filling-probes-...`.
> - **Item 2** (`GcNotificationThreadProbe`): the thread name passes on every collector; the only DIFF left was `notifications=<n>`, the collection count (HotSpot Serial 3 at `-Xmx256m`, Generational 7). That count cannot match by design today: Generational's young trigger fires at 50 % (moving) / 90 % (non-moving) of a quarter-heap semi-space (`young_trigger_percent`, `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT`), where Serial's eden is most of a third of the heap -- the eden-semantics direction is `gcd-d9b-proposal-young-trigger-counts-allocation-since-the-last-cycle-20260928.md`. **Landed:** `tools/probes/GcNotificationThreadProbe.java` prints `notifications=some` (or `0`) on stdout and the exact count on stderr (`[probe] notifications=<n>`); the verdict is unchanged.
> - **Run (orchestrator; refresh the HotSpot cache entry for this probe first -- its stdout changed):** `timeout 300 cratonvm -XX:+UseGenerationalGC -Xmx256m -cp out GcNotificationThreadProbe`, both modes, 3x. Expected (= HotSpot Serial): `notifications=some reentered=0 threads=[Notification Thread] PROBE-OK`. Retire items 1-2 on that; item 3 stays (lane f).

> **STATUS (2026-09-29, gc defects round, orchestrator, wave d10 verification on the Linux release build `cratonvm-gcd-d10` (`b30b8abaa`) against wave d9's `cratonvm-gcd-d9`, same load, two runs per row per collector (`docs/internal/gc-defects-round-20260927/verify-d10/vd10.list`)):**
> - **Item 2 is FIXED on every collector.** `GcNotificationThreadProbe` prints `threads=[Notification Thread] PROBE-OK` on Generational, G1 and ZGC; d9 printed `[Craton Finalizer]`. The remaining DIFF is only the `notifications=` count (HotSpot 3, Generational 7), which depends on the collection count.
> - **Item 1 is FIXED on Generational and ZGC.** `--compatible -Xmx64m FullHeapResolveProbe` prints `linked=6 oome=0 PROBE-OK`. The DIFF is in identity-hash values only. G1 fails.
> - **Item 3 is OPEN on all three collectors.** `NativeGrowthReclaimProbe -Xmx128m` gives rc 1 with `alCapacity=4/4 alAdd=4/4 toCharArray=1/4 sbCapacity=3/4` on Generational. With `CRATONVM_JIT_OSR=0` it matches HotSpot on Generational and ZGC, which points at the OSR holder page. d9 behaves the same.
> - **G1 fails before printing anything.** Its rows for this probe, `FullHeapResolveProbe`, `GenR4W4NativeStringOomProbe` and `GenR4W4HeapFullThrashProbe` exit with an uncaught `OutOfMemoryError: Java heap space` that has no Java frames, before any stdout line. d9 does the same. See `gcd-d10v-g1-heap-filling-probes-exit-before-any-output-with-an-uncaught-oome-20260929.md`.

> **STATUS (2026-09-28, gcd d10/f third commit, item 3 only; items 1 and 2
> as d10/o's status below): item 3 is OPEN, narrowed by reading, holder not
> yet named; no code change.** `main`'s OSR body is a SINGLE-PASS body
> (`ldiv` -> `osr_single_pass_preferred`). The interpreter frame's entry
> locals are parked (`CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`, default on). The
> Rust seed vector is not a root. Slot 10's single-pass home is rewritten
> (`istore 10`, bci 295) before the failing collections at bci 318. The
> leading candidate is a dead operand-spill word of the OSR body: the
> inline-cache args buffer that `l.add(filler)` / `l.size()` staged at
> depth 0 / 1, which the later `lload_3` does not rewrite, because a
> register-mapped local's push writes no frame word. The census command
> that names or refutes it, and a proposed fix, are in
> `gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`.
> Retire item 3 when `NativeGrowthReclaimProbe` prints HotSpot's line
> (`alCapacity=4/4 alAdd=4/4 toCharArray=4/4 sbCapacity=4/4 PROBE-OK`,
> `-Xmx128m`) 3/3 on Generational and ZGC. The G1 row is gated by
> `g1-humongous-refusal-with-free-space-and-old-array-fixup-misses.md`.
> The d10/o shared-door changes this page also names are completed in lane
> f's files by the same commit: `NativeContextImpl::reclaim_before_alloc_retry`
> now runs `native_reclaim_before_alloc_retry`, and G1's latched-overhead
> marking cycle runs in the JIT helpers and the exception builder.

> **Earlier status (2026-09-28, gcd d10/o): item 2 FIXED (thread name), awaiting the run; item 1 passes on Generational and ZGC, its G1 row is the G1 page's; item 3 is NOT the allocation door -- the data is HELD, by the OSR'd `main` (JIT, lane f), measured below. The shared doors now run every collector's full collection before an overhead-limit error (G1 was missing), landed default on.**
>
> - **Item 2, `GcNotificationThreadProbe` (fixed, all three collectors).**
>   Measured before the fix (pre-round binary `target/release/cratonvm.exe`
>   of 2026-09-27 15:59, Windows, `-Xmx256m`, both modes): Generational
>   `notifications=8`, G1 `1`, ZGC `2`, always `reentered=0 threads=[Craton
>   Finalizer] PROBE-OK`. HotSpot (Temurin 25.0.3, Serial / G1 / ZGC):
>   `notifications=3|2|26 reentered=0 threads=[Notification Thread]
>   PROBE-OK`. The name was ours: the listeners ran on the reference-delivery
>   thread, named `Craton Finalizer`. Landed:
>   `vm/src/runtime/interpreter/gc_and_alloc.rs`,
>   `run_gc_notifications_as_notification_thread` (+ `set_delivery_mirror_name`),
>   called from `drain_finalizer_thread_batch` at all three delivery points: for
>   the delivery only, the thread's Rust name and its Java mirror's `name` field
>   read `Notification Thread`, then are put back (a `finalize()` on that
>   thread still sees `Craton Finalizer`). Default on in both modes (a wrong
>   `Thread.getName()` answer); `CRATONVM_FINALIZER_THREAD=0` (the inline
>   arm) never reaches it. The count differs from HotSpot by design (one
>   notification per door pause; ZGC's bean shapes are
>   `gcd-d10o-g1-and-zgc-gc-log-and-mxbean-shapes-differ-from-hotspot-20260928.md`).
> - **Item 1, `FullHeapResolveProbe` (-Xmx128m, pre-round binary, both
>   modes):** Generational and ZGC `linked=6 oome=0 PROBE-OK`; G1 exits 1 with
>   an uncaught `OutOfMemoryError` before `round 0` (instrumented: the
>   `println` right after the fill's catch). The fill's 512 KiB `tail` array
>   is humongous on G1 and is refused with `0 free region(s)`, and this G1
>   has no compacting full collection to recover regions, where HotSpot's G1
>   runs a `Pause Full`: symptom 1 of
>   `g1-humongous-refusal-with-free-space-and-old-array-fixup-misses.md`
>   (G1-specific, not this wave). Retire item 1 on the Generational and ZGC
>   rows; the G1 row moves to that page.
> - **Item 3, `NativeGrowthReclaimProbe` -- the door is not the cause.**
>   By reading, every door on the failing path runs its collector's full
>   collection before it throws: the compiled `new byte[pieceBytes]`
>   (`jit_newarray_body`) forces a collection, then on Generational runs at
>   least one major in both arms (latched: the soft rung's major and
>   `majors_to_decide_oome`; unlatched: `jit_g1_last_ditch_full_cycle` =
>   `last_ditch_reclaim` + `second_major_before_oome`) -- the orchestrator's
>   d9 run shows `oldsz_true_root_majors=24`, all seeded from the true roots;
>   on ZGC the forced cycle is whole-heap; on G1 the latched arm without soft
>   references threw with no marking cycle (fixed below for the interpreter
>   and native doors; the JIT half is a cross-lane request to lane f). What
>   fails is that the data survives those collections. Row split (new
>   optional argument, see the probe's tail; pre-round binary, Windows,
>   `-Xmx128m`):
>
>   | rows | Generational | ZGC | Generational `CRATONVM_JIT_OSR=0` | ZGC `CRATONVM_JIT_OSR=0` |
>   |---|---|---|---|---|
>   | all four | OOME at `:95`/`:131` | OOME at `:95`/`:131` | -- | -- |
>   | `alAdd,sbCapacity` | OOME at `:95`/`:131` | OOME at `:95`/`:131` | `alAdd=3/4`, `sbCapacity=4/4` | `4/4`, `4/4` PROBE-OK |
>   | `alCapacity,sbCapacity` | PROBE-OK | PROBE-OK | PROBE-OK | PROBE-OK |
>   | `sbCapacity` alone | PROBE-OK | -- | -- | -- |
>
>   and `CRATONVM_JIT_DENY=NativeGrowthReclaimProbe.main` moves the failure
>   into the `alAdd` row (`alAdd=1/4`, `sbCapacity=4/4`) while denying
>   `fillAndDrop` changes nothing. So the holder is the `alAdd` row's
>   `ArrayList l` (26 %, grown to ~39 % by its last `add`), kept by the
>   OSR-compiled `main` (entered at the `l.add(filler)` loop) past the row,
>   on Generational AND ZGC -- a common JIT-frame root, not the door and not
>   the Generational seed. Owner: lane f (frames), with
>   `gengc-r5w6-oomjit10-proposal-single-pass-local-liveness` and the OSR
>   entry-locals history below. The d10 build must confirm the split (the
>   binary above predates the round).
> - **Landed on the shared doors (default on, `=0` switch
>   `CRATONVM_GC_OVERHEAD_PROGRESS=0`):** `g1_overhead_limit_full_cycle` --
>   under a latched overhead streak G1 now runs `last_ditch_reclaim`'s marking
>   cycle and honours the verdict only if it freed under 2 % of the heap
>   (interpreter: `collect_and_retry_with_thread`'s unjudged `Attempt` arm);
>   and `native_reclaim_before_alloc_retry`, the native factories' ladder
>   with the interpreter's rungs (majors on Generational, the G1 cycle, the
>   second major), live once lane f points
>   `NativeContextImpl::reclaim_before_alloc_retry` at it (cross-lane
>   request in `docs/internal/gc-defects-round-20260927/d10-o-report.md`).
>   Neither changes item 3's shape: its collections run and free nothing
>   held.
>
> **Run (orchestrator, each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`,
> `-XX:+UseZGC`, both modes, 3x):**
> ```
> javac -d out tools/probes/GcNotificationThreadProbe.java \
>     tools/probes/FullHeapResolveProbe.java tools/probes/NativeGrowthReclaimProbe.java
> for gc in UseGenerationalGC UseG1GC UseZGC; do for mode in "" --compatible; do
>   timeout 300 cratonvm $mode -XX:+$gc -Xmx256m -cp out GcNotificationThreadProbe
>   timeout 300 cratonvm $mode -XX:+$gc -Xmx128m -cp out FullHeapResolveProbe | tail -1
>   timeout 300 cratonvm $mode -XX:+$gc -Xmx128m -cp out NativeGrowthReclaimProbe; echo "rc=$?"
>   timeout 300 cratonvm $mode -XX:+$gc -Xmx128m -cp out NativeGrowthReclaimProbe alAdd,sbCapacity; echo "rc=$?"
>   CRATONVM_JIT_OSR=0 timeout 300 cratonvm $mode -XX:+$gc -Xmx128m -cp out NativeGrowthReclaimProbe alAdd,sbCapacity; echo "rc=$?"
> done; done
> ```
> Expected: `GcNotificationThreadProbe` prints `notifications=<n>
> reentered=0 threads=[Notification Thread] PROBE-OK` with `n >= 1` on all six (retires
> item 2); `FullHeapResolveProbe` `linked=6 oome=0 PROBE-OK` on Generational
> and ZGC (retires item 1; G1 still exits 1 before `round 0` -- the G1 page);
> `NativeGrowthReclaimProbe` still fails on Generational and ZGC in the same
> place, and the split reproduces the table (the `OSR=0` arm passes
> `sbCapacity=4/4`) -- then item 3 goes to lane f as the OSR'd `main`
> holder; on G1 it fails before any row (the G1 page). HotSpot (`-XX:+UseSerialGC`
> / `UseG1GC` / `UseZGC`, measured): the four `4/4` rows `PROBE-OK (sink 0)`,
> and with `alAdd,sbCapacity`: `alCapacity=skipped alAdd=4/4
> toCharArray=skipped sbCapacity=4/4 PROBE-OK (sink 0)`. The census that names
> the holder on Generational: `CRATONVM_DBG=oldmark-root-census,root-source
> CRATONVM_DBG_JIT_ROOTSCAN=1 ... NativeGrowthReclaimProbe alAdd,sbCapacity`,
> last major before the error.

> **Earlier status (2026-09-28, gcd d9/a): item 3 OPEN, holder still unnamed; the likeliest holder (the true-root major's legacy-seed fallback) is FIXED this wave, default on, and the run below tells whether it was this one. Items 1-2 unchanged (pass on Generational; G1 / ZGC rows not run).**
>
> - **Where d8/x left item 3 (`307f0c6a2`):** `NativeGrowthReclaimProbe -XX:+UseGenerationalGC -Xmx128m` failed 6/6 in both modes (uncaught `OutOfMemoryError` at `main` line 95, `fillAndDrop` line 131, the `sbCapacity` rounds' `fillAndDrop(max * 70 / 100)`), `CRATONVM_GC_OVERHEAD_PROGRESS=0` the same. No census was run on it.
> - **Is it the pinned-callee / thread-exit holder? By reading, plausibly:** the failing allocation is a compiled `new byte[pieceBytes]`, whose helper answers a latched overhead streak with a REQUESTED major (d1/b, `jit_latched_overhead_limit_throws`); each earlier round leaves `junk` arrays and pieces spread over young and old, and a requested major whose young phase promoted anything (likely at 58-70 % occupancy) kept the legacy seed, i.e. every promotion destination and every young survivor rooted -- the `cat=seed/promotion-dest` holder of `gcd-d6u-pinned-callee-oome-heap-stays-full-after-the-catch-20260928.md`. gcd d9/a keeps the true-root seed on such a cycle (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE`, default on; details on that page). Nothing here proves the holder; the A/B below does.
> - **Run (orchestrator, Linux release, JIT on):**
>   ```
>   javac -d out tools/probes/NativeGrowthReclaimProbe.java
>   for i in 1 2 3; do for mode in "" --compatible; do
>     CRATONVM_DBG=gc-stats timeout 300 cratonvm $mode -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe 2>ngr.err; echo "[$mode] rc=$?"; grep -h 'oldsz_true_root' ngr.err | tr ' ' '\n' | grep true_root
>   done; done
>   CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0 timeout 300 cratonvm -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe; echo "[wide=0] rc=$?"
>   CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG=1 timeout 300 cratonvm -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe; echo "[step1] rc=$?"
>   ```
>   Expected (HotSpot 25 `-XX:+UseSerialGC`, checked 2026-09-28, and `--nojit`): `NativeGrowthReclaimProbe alCapacity=4/4 alAdd=4/4 toCharArray=4/4 sbCapacity=4/4 PROBE-OK (sink 0)`, rc 0 -- 3/3 in both modes, with `oldsz_true_root_fallbacks=0`. Reading the arms: default passes and `WIDE=0` fails = it was this holder (retire item 3); both fail with `fallbacks=0` = not this holder, run the census (`CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JIT_ROOTSCAN=1`, the last major before the error; its block now says `seed=true-roots` when the seed ran) and send the named holder to its owner; only the step-1 arm passes = the young half (`gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-20260928.md`).
> - **Items 1-2:** unchanged from d8/x (`FullHeapResolveProbe --compatible` `linked=6 oome=0 PROBE-OK`, `GcNotificationThreadProbe --compatible` `notifications=7 reentered=0 threads=[Craton Finalizer] PROBE-OK` on Generational); G1 / ZGC rows still to run.

> **STATUS (2026-09-27, gcd d1/b): item 3 NARROWED -- the remaining failure
> is most likely not a holder but the JIT allocation helpers' latched
> overhead limit; FIX LANDED, default on, awaiting the probe.** Items 1 and 2
> are unchanged (w36-e; see below).
>
> - **What still failed:** round-5 tip `toCharArray=1/4` (and before it the
>   uncaught OOME at line 95). Both are allocations in COMPILED
>   `fillAndDrop` (`NativeGrowthReclaimProbe.java:131`, `new byte[pieceBytes]`,
>   `jit_newarray_body`): while a round fills 58-70 % of the heap with live
>   pieces, every forced young cycle frees nothing and the old generation fills,
>   so the GC-overhead streak LATCHES; the previous rounds' dropped data sits in
>   the old generation below the 75 % major floor. The helper then answered the
>   latched streak with the soft-reference rung only and, the probe holding no
>   `SoftReference`, threw at once -- no major ever ran. `--nojit` passes
>   because the interpreter ladder runs a major before honouring the streak
>   (gen r4w4); `CRATONVM_JIT_OSR=0` passed because it leaves `fillAndDrop`'s
>   loop interpreted. Every census holder earlier waves named (the OSR-left
>   interpreter frame, `main`'s dead homes) is fixed; none of them explains
>   `toCharArray` failing after round 1.
> - **Landed:** `vm/src/jit/helpers.rs`, `jit_latched_overhead_limit_throws`
>   (+ `jit_overhead_limit_major`, `jit_overhead_limit_verdict`): a latched
>   streak with no soft-reference collection runs a Generational major on this
>   thread first and throws only if that major leaves the streak latched; a
>   lost STW race is no verdict. `=0` switch: `CRATONVM_GC_OVERHEAD_PROGRESS=0`.
>   Unit test `jit::helpers::gcd_d1b_overhead_verdict_tests`.
> - **Still open here:** nothing else known. If the probe still fails, the
>   census (`CRATONVM_DBG=oldmark-root-census,root-source
>   CRATONVM_DBG_JIT_ROOTSCAN=1`) names the holders of the largest old objects
>   (`auto:` markers); a band word's `prov=` now also says ` tier=ir|sp ...
>   in_map=` (`conservative_roots::band_word_context`).
>
> **Run (orchestrator):**
> ```
> javac -d out tools/probes/NativeGrowthReclaimProbe.java
> for i in 1 2 3; do
>   for mode in "" --compatible; do
>     cratonvm $mode -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe
>   done
> done
> CRATONVM_GC_OVERHEAD_PROGRESS=0 cratonvm -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe
> ```
> Expected at default, both modes, 3/3 (= `--nojit` and HotSpot Serial):
> `NativeGrowthReclaimProbe alCapacity=4/4 alAdd=4/4 toCharArray=4/4
> sbCapacity=4/4 PROBE-OK (sink N)`, exit 0. The `=0` run is the comparison
> (the round-5 tip's `toCharArray=1/4` or the line-95 OOME). Retire item 3
> when the default runs pass; the page retires with it (items 1 and 2 await
> only their w36-e runs).

> **Earlier status (2026-09-27, gen r5w6/oomjit10): item 3 -- the fix for the
> OSR'd `main`'s OWN frame LANDED, default on, awaiting the probe.** On the
> wave-5 staging build `NativeGrowthReclaimProbe -Xmx128m` still died at line
> 95 (`fillAndDrop(max * 70 / 100)` of the `sbCapacity` rounds) with the JIT,
> with and without the OSR-entry parking. By reading, the holders are the
> optimizing-tier OSR frame of `main` (OSR'd at the `alAdd` inner loop, the
> one hot back edge; the body covers the rest of `main`): the dead
> `toCharArray` round's `s` (15 %) and `cs` (30 %) stay in homes the keep set
> called live -- every later frame state named their local slots although
> bytecode liveness says they are dead, and the loop-carried slot values sit
> in PHI homes, which were never cleared -- so 45 % of the heap survives into
> a round that allocates 70 %. Landed (`jit/src/ir_lower.rs`,
> `CRATONVM_JIT_IR_PRECISE_KEEP_SET`, default on, `=0` off): snapshot locals
> narrowed by handler-aware bytecode liveness, phi homes clearable, and the
> keep set a liveness with kills. The census also explains a probe like this
> one now (no `WeakReference` needed): with no staged marker it names the
> holders of the largest root-reached old objects.
>
> **Run (orchestrator):**
> ```
> javac -d out tools/probes/NativeGrowthReclaimProbe.java
> for mode in "" --compatible; do
>   cratonvm $mode -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe
> done
> CRATONVM_JIT_IR_PRECISE_KEEP_SET=0 cratonvm -XX:+UseGenerationalGC -Xmx128m \
>   -cp out NativeGrowthReclaimProbe            # the wave-5 answer: the OOME at line 95
> CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JIT_ROOTSCAN=1 \
>   cratonvm -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe 2>census.log
> grep -n '^\[holder-census\]' census.log
> ```
> Expected at default, both modes (= `--nojit` and HotSpot Serial):
> `NativeGrowthReclaimProbe alCapacity=4/4 alAdd=4/4 toCharArray=4/4
> sbCapacity=4/4 PROBE-OK (sink N)`, exit 0. If it still fails: a
> `marker#... auto: ...` holder line with `region=java-local` at a
> SINGLE-PASS `main` frame means the OSR body was not optimizing-tier
> (item 5 of `gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927.md`; proposal
> `gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md`); a
> `cat=...interp-local-...` holder is the interpreter frame, not compiled code.
> ZGC is not re-run here (out of this round's scope).

> **Earlier status (2026-09-27, gen r5w5/oomjit9): item 3's `try_osr` edit is now
> DEFAULT ON, awaiting the probe.** Measured on `d8a690353` (Linux release):
> `NativeGrowthReclaimProbe -Xmx128m` dies of `OutOfMemoryError: Java heap
> space at NativeGrowthReclaimProbe.main(NativeGrowthReclaimProbe.java:95)`
> with the JIT in both modes and passes `--nojit`. Landed:
> `osr_park_entry_locals_enabled` (`vm/src/runtime/interpreter/jit_bridge.rs`)
> reads `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS` as default-on (`=0` restores the
> old behaviour), so an OSR entry takes the reference locals out of the
> interpreter frame it leaves behind -- the `interp-local-live` root of the
> census below -- exactly as the gen r5w1 opt-in did (the design, the replay
> refusals and the declined-entry restore are unchanged). Also landed, for
> the OSR'd `main`'s OWN frame: the optimizing tier's default dead-home
> clears (`CRATONVM_JIT_IR_DEAD_HOME_CLEARS`, see
> `../../internal/gc/gengc-r5w1-oom5-jit-oome-retention-regressed-on-dev-9e252c8b2-FIXED-20260928.md`).
> Cross-lane (flag inventory owner): the row `osr-park-entry-locals` in
> `types/src/flag_groups.rs` should now carry `off_word: Some("0")`.
>
> **Run (orchestrator):**
> ```
> javac -d out tools/probes/NativeGrowthReclaimProbe.java
> for mode in "" --compatible; do
>   cratonvm $mode -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe
>   cratonvm $mode -XX:+UseZGC -Xmx128m -cp out NativeGrowthReclaimProbe
> done
> CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS=0 cratonvm -XX:+UseGenerationalGC -Xmx128m \
>   -cp out NativeGrowthReclaimProbe            # the old answer: the OOME
> CRATONVM_DBG_JITC=1 cratonvm -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe
> ```
> Expected at default, every arm (= `--nojit` and HotSpot Serial):
> `NativeGrowthReclaimProbe alCapacity=4/4 alAdd=4/4 toCharArray=4/4
> sbCapacity=4/4 PROBE-OK (sink N)`, exit 0; the `CRATONVM_DBG_JITC=1` run's
> unresumable-exit / replay-refusal counters stay at zero. A residual
> `alAdd` miss with the census naming a `region=operand-spill` word of `main`
> is item 2 of `gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927.md` (a
> loop-carried `l` in a phi home).

> **Earlier status (2026-09-26, gen r5w1/oom5): item 3's remaining edit LANDED,
> opt-in `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS=1` (default off), awaiting the
> probe.** `try_osr` (`vm/src/runtime/interpreter/jit_bridge.rs`), after
> admission and just before `set_jit_thread`, replaces every non-null
> reference local of the frame it leaves behind with null
> (`park_osr_entry_reference_locals`; the kind-honouring tag first, so a
> colliding `long`/`double` is never taken; the setter bumps `exec_epoch`, so
> a cached root snapshot is not reused). It differs from the design below in
> one deliberate way: the values are NOT restored after the activation.
> Every exit that keeps the frame writes the locals it needs (the OSR-exit,
> guard-exit and exception-handler transfers; admission refuses artifacts
> with `Unsupported` slots), a return pops the frame, and a propagating
> exception unwinds it. Restoring after the activation by a collection-count
> epoch would trust raw addresses across Java code that ran and reached
> safepoints; not restoring needs no such trust. The
> readers of the pre-entry locals are the replay arms, which now REFUSE
> (`osr_refuse_replay(.., replay_forbidden)`: `InternalError`, never a replay
> on nulls): the panic arm, the transfer-refused arm, and the foreign-stash
> and frameless arms even under `CRATONVM_JIT_OSR_LOUD_UNRESUMABLE=0`. The one
> arm where nothing ran (`osr_enter` / `ir_osr_enter` declined) puts the
> parked words back when no collection ran since parking (no Java ran and
> this thread polled no safepoint in that window) and refuses otherwise. x86-64 only (the one target that enters OSR bodies). Unit tests:
> `jit_bridge::r5w1_oom5_osr_park_tests`. Known residue: the three "could not
> build the NPE / AIOOBE / ArithmeticException object" arms still resume at
> `entry_pc` (bootstrap-only; they would run on nulls with the flag), and a
> JVMTI `GetLocal*` on the OSR'd frame reads null during the activation.
>
> **Probe:** `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS=1` with
> `NativeGrowthReclaimProbe`, Generational and ZGC `-Xmx128m`, both modes,
> JIT on: `4/4 x4 PROBE-OK`. With `CRATONVM_DBG_OLDMARK_ROOT_CENSUS=1` the
> `interp-local-live` row for alAdd round 1's `ArrayList` must be gone. Then
> the OSR battery with the flag before any default flip, and
> `CRATONVM_DBG_JITC=1`'s unresumable-exit counters must stay at zero.
>
> **STATUS (2026-09-26, gc-common w36-f, measured on
> `cratonvm-gccommon-w36` = `8d61f8220` without w36-f): PARTLY FIXED.
> Items 1 and 2 are fixed in the tree (w36-e), pending the orchestrator's
> probe run. Item 3, `NativeGrowthReclaimProbe`, has two roots: one is fixed
> by w36-f, the other needs the edit below in
> `vm/src/runtime/interpreter/jit_bridge.rs::try_osr`, which belongs to the
> interpreter / JIT-bridge owner, not this lane.**
>
> **Items 1 and 2 (w36-e, unchanged).** `FullHeapResolveProbe`:
> `handoff-w36e-jit-anewarray-collects-before-oome.md` is applied at
> `8d61f8220` (`jit_anewarray_object_body` collects once before it throws).
> `GcNotificationThreadProbe`: `gc_and_alloc.rs::forced_door_hands_off_gc_notifications`,
> test `w36e_the_forced_door_hands_off_notifications_under_a_held_lock`.
> Both retire on the orchestrator's run: `linked=6 oome=0 PROBE-OK` on
> `--compatible -XX:+UseZGC -Xmx128m`, and `notifications>0 ... PROBE-OK` on
> all three backends under `--compatible -Xmx256m`.
>
> **Item 3, `NativeGrowthReclaimProbe` (open).** It is not ZGC-only.
> Generational fails the same way (`OutOfMemoryError` at
> `NativeGrowthReclaimProbe.java:95` -> `fillAndDrop:131`), and so do G1 and
> ZGC `--compatible`. `CRATONVM_JIT_OSR=0` gives `4/4 x4 PROBE-OK` on ZGC.
> `CRATONVM_JIT_DENY=NativeGrowthReclaimProbe.main` fixes three rows
> (`alAdd=0/4`, the rest 4/4). Denying `fillAndDrop` fixes nothing. So the
> root is in OSR'd `main`, not in the allocating loop.
> `CRATONVM_DBG_OLDMARK_ROOT_CENSUS=1` on Generational names the two roots
> at every compiled-code OOME:
>
> | root (census label) | object | bytes | why |
> |---|---|---|---|
> | `interp-local-live` in `main`'s interpreter frame | the `ArrayList` of alAdd round 1 (`l`) | 34 896 648 (26 % of the heap) | the frame the OSR entry left behind; see below |
> | `native-pending-return` (`prov=main off=88`) | the `"x".repeat(..)` String of the toCharArray row | 20 132 680 | a native's object result handed to compiled `main`; **fixed by w36-f** (`common-w4o` cause 1, `applied/handoff-w36b-...`) |
>
> The same `ArrayList` address is the top root at every major from alAdd
> round 1 to the final OOME in the sbCapacity row. With 26 % of the heap
> pinned, every later `fillAndDrop` (45-70 %) plus the row's own live data
> exceeds `-Xmx128m`. This also explains the "older" Generational
> `alAdd=0/4`: round `n+1` holds its own `l` plus round 1's. ZGC runs the
> same VM-side root set (`roots.rs::collect_roots`); the census is
> Generational-only.
>
> **The OSR root.** `try_osr` enters the OSR body with a *copy* of the
> interpreter frame's locals (`jit_locals`) and leaves
> `thread.frames[frame_idx]` on the stack, locals unchanged, for the whole
> activation. The root scan still reads it:
> `scan_local_objects` uses liveness at `frame.pc` (the OSR entry bci, in
> alAdd's inner loop, where `l` is live), and under any JIT-active
> collection `conservative_locals_enabled` also scans every local
> liveness-blind. So whatever the frame held at entry stays reachable until
> the OSR'd method returns. For `main` that is the rest of the program.
> Moving `frame.pc` would not help, because of the liveness-blind scan; the
> values themselves have to leave the frame.
>
> **Remaining edit (owner: `jit_bridge.rs` / `deopt_resume.rs`).** In
> `try_osr`, just before `let saved_jit_thread = crate::jit::helpers::set_jit_thread(thread);`
> (the entry has been admitted and nothing has run), park the frame's
> reference locals *outside* the root set:
>
> ```rust
> // w36-f: the OSR body owns these now (it was seeded from `jit_locals`);
> // left in the frame they are roots for the whole activation.
> let parked_epoch = shared.mem.heap.collection_count();
> let parked: Vec<(usize, u64)> = {
>     let frame = &mut thread.frames[frame_idx];
>     (0..frame.locals_len())
>         .filter_map(|i| match frame.get_local_unchecked(i) {
>             Value::Object(Some(o)) => Some((i, o.as_ptr() as u64)),
>             _ => None,
>         })
>         .collect()
> };
> for &(i, _) in &parked {
>     thread.frames[frame_idx].set_local_unchecked(i, Value::Object(None));
> }
> ```
>
> Right after `crate::jit::helpers::restore_jit_thread(saved_jit_thread);`,
> restore them only while the raw words are still addresses:
>
> ```rust
> let parked_lost = !parked.is_empty()
>     && shared.mem.heap.collection_count() != parked_epoch;
> if !parked_lost {
>     for &(i, raw) in &parked {
>         // SAFETY: no collection ran since they were read out of the frame.
>         let o = unsafe { ObjectRef::from_raw(raw as *mut u8) };
>         thread.frames[frame_idx].set_local_unchecked(i, Value::Object(Some(o)));
>     }
> }
> ```
>
> The exits that overwrite the frame need nothing more.
> `transfer_osr_exit_into_live_frame` and its exception and guard siblings
> write every local the artifact publishes, and a dead one is written
> `Undefined`. A normal return pops the frame. The exits that *read* the
> pre-entry locals must refuse when `parked_lost` is set, because the
> pre-entry values are then gone. These are the replay arms: every
> `osr_refuse_replay` call (throw its `InternalError` unconditionally, not
> only when `bytecode_commits_side_effect`), the frameless-replay
> `return None` after `note_osr_unresumable_exit(.., &OSR_EXIT_FRAMELESS_REPLAYS, ..)`,
> and the foreign-stash one. They are documented as expected-zero, and
> `CRATONVM_DBG_JITC=1` prints their counters. The same applies to a
> transfer's `FrameValue::Unsupported` local ("leaves that slot's existing
> live value untouched"): with `parked_lost` it must refuse. Also audit
> any reader of an OSR'd frame's locals during the activation (JVMTI
> `GetLocal*`, `LiveStackFrame.getLocals`), which would now read `null`
> instead of the stale pre-entry value. That value was already wrong once
> the body had run. Suggested gate: `CRATONVM_JIT_OSR_PARK_ENTRY_LOCALS`,
> default ON after the orchestrator's OSR battery, with `=0` for bisecting.
> Unit-test it in `jit_bridge.rs`'s test module: park, then restore with
> an unchanged epoch; with a moved epoch, the replay arm refuses.
>
> **Also seen, not needed for this probe** (a split variant with each row in
> its own method, `NgrSplit`, Generational census): when a compiled frame is
> active, `conservative_locals_enabled` roots a local that liveness says is
> DEAD in an *interpreted* caller (the `char[]` of the previous toCharArray
> round, 40 MB, `interp-local-dead-by-liveness`). This is the documented
> price of the liveness-blind scan under the non-moving sweep
> (`roots.rs::conservative_frame_pass`). The same run once rooted a
> `StringBuilder` of a previous round through the collecting thread's own
> registry snapshot (`registry snapshots (every alive thread, this one
> included)`). The original probe does not reach either, because `main` is
> its only interpreter frame.
>
> **What retires item 3:** `NativeGrowthReclaimProbe` `4/4 x4 PROBE-OK` on
> ZGC and Generational `-Xmx128m`, both modes, JIT on. Expected once the
> edit above lands; w36-f's native-return fix alone is not enough, since
> the census shows the `ArrayList` pinned without it.

- **Status (original):** OPEN. Filed 2026-09-26 by the gc-common round orchestrator, while
  verifying the round's merge of `origin/dev`.
- **Kind:** regressions that dev brought in. The round's own code is not the
  cause: a release build of `origin/dev` alone (`f144bb3d2`, built with nothing
  from this branch) fails all three probes in exactly the same way.
- **Suspects:** the 8-byte object header (`2ea2e5921` and its neighbours, e.g.
  `GC_FLAG_HEADER`) and the generational `gen r4` plumbing (`fa1e9df43`). This
  has not been bisected.

## Evidence

The probes are the round's battery set: `probes9/NativeGrowthReclaimProbe`,
`probes6/GcNotificationThreadProbe` and `probes9/FullHeapResolveProbe`.

| Probe | Flags | `cratonvm-gccommon-w32` (before the merge) | `origin/dev` `f144bb3d2` | w33m / w34 (after the merge) |
|---|---|---|---|---|
| `NativeGrowthReclaimProbe` | `-Xmx128m`, Generational / ZGC, both modes | `PROBE-OK` on ZGC; Generational `alAdd=0/4` (a known, older gap) | `OutOfMemoryError: Java heap space` at `fillAndDrop` (`NativeGrowthReclaimProbe.java:131`) | the same OOME |
| `GcNotificationThreadProbe` | `--compatible -Xmx256m`, all three backends | `notifications=8` (Gen), `1` (G1), `2` (ZGC), delivered on `Craton Finalizer` | `notifications=0 ... NO-NOTIFICATIONS` | the same |
| `FullHeapResolveProbe` | `--compatible -XX:+UseZGC -Xmx128m` | `linked=6 oome=0 PROBE-OK` | fails at `FullHeapResolveProbe.java:17`, rc=1 | the same |

In `--jdk-only`, `GcNotificationThreadProbe` still passes on all three backends,
and `FullHeapResolveProbe` passes on Generational and ZGC. The G1 `-Xmx128m`
refusals are older and unchanged.

On the ZGC OOME, the arena reports fragmentation, not exhaustion:
`largest_free_block=3944` for a 4112-byte request, and 1022 walls of about
128 KiB of `java/lang/Object`.

## Reproduce

```bash
javac -d out tools/probes/NativeGrowthReclaimProbe.java \
    tools/probes/GcNotificationThreadProbe.java tools/probes/FullHeapResolveProbe.java
cratonvm -XX:+UseZGC -Xmx128m -cp out NativeGrowthReclaimProbe
cratonvm --compatible -XX:+UseGenerationalGC -Xmx256m -cp out GcNotificationThreadProbe
cratonvm --compatible -XX:+UseZGC -Xmx128m -cp out FullHeapResolveProbe
```

Build `92ad6b756` and `f144bb3d2` and bisect between them. The probe sources
live in the orchestrator's scratch set, not in the repository. The first two
are small, and `tools/probes/` is the right home for them.

## What retires this page

All three probes pass again on dev, or each regression is explained as
intended (for example, a heap-accounting change that the probe's `-Xmx` must
follow) and the probe is updated to match.
