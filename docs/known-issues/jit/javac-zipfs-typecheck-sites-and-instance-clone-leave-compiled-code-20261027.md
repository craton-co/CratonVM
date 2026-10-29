# In-process `javac` on jar classpaths: type-check sites and `Object.clone` leave compiled code (2026-10-27)

> **j27 triage (builder lane TRIB, 2026-10-28, Windows, release LTO-off build of the j27 tree at 20:34 = `C:\craton\j27-trib-bin\cvm-trib-1.exe`): item 2 VERIFIED, retire it; item 1 NOT REPRODUCED on Windows.** `ZipFsFinalizerChurn <40 ~/.m2 jars> 5` under `CRATONVM_DBG=jitc` prints 0 `inline-resolve REFUSED ... checkcast/instanceof [tier=sp` lines, and 87 come back with `CRATONVM_JIT_SP_SPLICE_TYPECHECK=0`. Item 1: `CRATONVM_DBG_TYPECHECK_FILTER=/` prints 0 census lines on that probe, and 0 on an in-process `com.sun.tools.javac.Main` run over 162 probe sources with the same 40 jars on `-cp` (stopped after the jitc log reached 1.3 GB). The census itself works: `=Tc` on a default-package test prints `census op=instanceof target=TribTcCensus$I receiver=0:java/lang/Object reason=array-receiver`. Recipe note: `/` matches only targets in a package. Item 1 still needs the Spring-suite javac on the host.

> **j27 wave 8 (lane W8DSP, 2026-10-28): NOT FIXED (unchanged), PENDING HOST VERIFICATION.** Item 2 is built
> (default ON, host check of wave 6 still due); item 3 is CLONE / CLONE2's, in code; item 1 is still
> NEEDS-MEASUREMENT: wave 3's census (`CRATONVM_DBG_TYPECHECK_FILTER=/`) and wave 6's triage recipe below
> decide which screen declines, and each answer is a different fix in a different place. No code written
> blind. Re-checked by reading that the census fields wave 6's recipe greps (`op=`, `target=`, `receiver=`,
> `reason=`, `answer=`) are the ones wave 3's line prints; the recipe is runnable as written.

> **j27 wave 6 (lane T1LOOPSTATIC, 2026-10-28): NOT FIXED (classification and triage recipe only, no code), PENDING HOST VERIFICATION.**
> I checked items 1 and 2 against the current tree. `git diff` shows no edit this round to
> `vm/src/jit/helpers/typecheck.rs` or `jit/src/typecheck_rows.rs`. Every DISPATCH21 row is already
> implemented opt-in (`docs/internal/proposals/jit/tier1/jit-r21-dispatch-proposals.md`), so there was
> nothing to overlap with. There was also nothing to build either.
> * **Item 2: ALREADY BUILT** (TCARRAY's correction above holds). `CRATONVM_JIT_SP_SPLICE_TYPECHECK` is
>   default ON (`jit/src/ir.rs` `sp_splice_typecheck_enabled`, read by the `0xc0 | 0xc1` arm in
>   `vm/src/runtime/interpreter/jit_bridge.rs`). Host check: 0 `inline-resolve REFUSED ... checkcast/instanceof [tier=sp`
>   lines under `CRATONVM_DBG=jitc` on `ZipFsFinalizerChurn`, and the lines come back under `=0`. Then
>   retire the item.
> * **Item 1: NEEDS MEASUREMENT.** The fix depends on which inline screen declines, and only wave 3's
>   census can tell. Each reason means a different fix in a different place, so writing one blind would
>   be a guess. The helper's own cost (`is_object_address`, the stale-reference guard documented in
>   `jit_instanceof_body`) is a safety net and stays.
> * **Item 3:** CLONE / CLONE2's (waves 3 and 4); unchanged here.
>
> **Triage recipe for item 1** (one `--jdk-only` run of the probe, plus the javac class if the probe is
> silent):
> ```text
> javac -d <out> tools/probes/spring/ZipFsFinalizerChurn.java
> CRATONVM_DBG=jitc CRATONVM_DBG_TYPECHECK_FILTER=/ cratonvm --jdk-only -cp <out> ZipFsFinalizerChurn 2> tc.log
> grep ' census ' tc.log | sed 's/ site=[^ ]*//' | sort | uniq -c | sort -rn | head -40
> grep -E 'ir instanceof display arm|row-first arm' tc.log | grep -E 'IndexNode|ZipUtils|FileTreeWalker|getFileAttributes'
> ```
> Read the top rows (target `jdk/nio/zipfs/ZipFileSystem$IndexNode` first) by `reason` (most lines
> first):
> * `no-display` repeated for ONE receiver class: the helper's lazy publish
>   (`access.rs` `vh_const_ensure_display`) fails for that class every time. File a page naming the
>   class and why `vh_const_publish_display` refuses it. A single line per receiver is the expected
>   first call and needs nothing.
> * `display-verdict-0`: the receiver's chain is not hierarchy-only
>   (`supertype_closure_is_hierarchy_only`), so every miss comes here by design. Name the
>   non-hierarchy class on the chain; the fix is in the verdict, not the site.
> * `display-hierarchy-only`: the inline arms had the facts and the SITE declined. Compare with that
>   site's `jitc` lines:
>   * `display arm DECLINED`: the arm's admission screen; fix it there.
>   * the site was compiled before the receiver's display existed (no `DECLINED` line for that site,
>     and its compile line comes before the receiver's first census line): measure the opt-in `CRATONVM_JIT_TYPECHECK_ROWS=1` /
>     `CRATONVM_JIT_TYPECHECK_ROWS_ALL=1` arms (DISPATCH21-1), which read a row at run time.
>   * an interface target: `CRATONVM_JIT_TYPECHECK_ROWS=1`.
> * `array-receiver`: the TCARRAY page `j27-typecheck-array-arm-and-array-miss-switches-are-dead-20261028.md`.
>
> For the rows arms: A/B against default on the probe and on the javac class below, pinned, interleaved,
> 5 reps, medians. Count `jit_instanceof` with the census line count, and take the wall time from the
> probe. A flip needs every `fact` line SAME and no other probe row regressing.
> Javac wall check (host, G1, `-Xmx4g`, load < 8):
> `AutowiredAnnotationBeanRegistrationAotContributionTests` through the Spring Framework suite harness
> (memory note "Spring Framework suite on the host"), compared with HotSpot's 21-26 s.

> **j27 wave 4 (lane CLONE2, 2026-10-28): PARTLY (item 3 extended to spliced bodies and two wave-3 defects fixed; items 1 and 2 unchanged), PENDING HOST VERIFICATION.**
> **Spliced instance sites** (proposal item 1 of `j27-clone-spliced-instance-sites-and-inline-expansion-proposal-20261028.md`):
> after a splice, `clone_thin_bind::bind_served_spliced_clone_calls` (`jit/src/clone_thin_bind.rs`, called from the
> optimizing tier's splice loop in `jit/src/lib.rs` beside the `ThreadLocal` unbind pass) asks wave 3's bind question of
> every surviving `invokevirtual ...clone()Ljava/lang/Object;` with the CALLEE's receiver counts for that call
> (`IrInlineTables::spliced_receiver_seeds`); a served dominant class turns the row into the thin helper's direct row
> and drops its inline cache. Switch `CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND` (default ON, `=0` restores wave 3:
> arrays only in a splice), under `CRATONVM_JIT_CLONE_VIRTUAL_THIN`. Same evidence rule and decline as an own site, so
> a wrong bind costs a generic dispatch per call, never an answer.
> **Review fixes (no switch).** (1) The verdict behind both the compiler's bind question
> (`jit_object_clone_served_class`, run on the COMPILE thread) and the helpers' per-receiver screen took a BLOCKING
> `class_manager.read()`; it is now `try_read_untracked` (the `jit_well_known_class_id` discipline): busy answers "not
> served" and is not memoised (`object_clone_class_verdict_by`). (2) Every clone helper also declines while a JVMTI
> `SampledObjectAlloc` listener is armed (`clone_vm_object_alloc_armed`), as it already did for `VMObjectAlloc`.
> Reviewed and found sound: reference-array barriers (`try_reference_arraycopy_pair` on a fresh same-class copy: the
> collector's raw arm or the per-element accessors), the orphan zeroed TLAB object on a decline after the bump (a
> parseable dead object, announced to ZGC, charged once; perf only; the class-level refusals are now asked before
> the bump, see proposal item 3), JFR (no allocation event is
> emitted on any TLAB bump, inline `new` included), the shared kind-0 `java/lang/Object` decline descriptor (arrays
> select `Object`'s table in `virtual_dispatch_target_for_receiver`, the lean door keys records by exact receiver
> class and declines arrays, the access guard skips a `declaring_class_id` 0 info), and the ABI counts (200 words,
> 87 rows, version 28: the const asserts and the comment trail agree).
> **Host must confirm:** (a) `tools/probes/jit/j27/J27Clone2Rows.java` prints its header facts under the default flags,
> `--nojit`, `CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND=0` and G1 / Generational / ZGC, and `J27CloneRows` is unchanged;
> (b) `CRATONVM_DBG_JITC=1` on `J27Clone2Rows` shows `clone-virtual-bind door=splice-seed class=J27Clone2Rows$Point
> dominant=Some(..) Served` (absent under `=0`); (c) unit tests `cratonvm-jit`
> `clone_thin_bind::j27_clone_virtual_thin_tests::j27_a_spliced_instance_clone_binds_from_the_callee_profile` and
> `cratonvm-vm` `jit::helpers::j27_clone_copy_tests::{j27_a_busy_class_manager_answers_not_served_without_waiting,
> j27_a_busy_verdict_is_not_memoised_and_a_computed_one_is}` (the first hung before the fix), with
> `--features experimental-debug` too for the sampled-alloc arm.

> **j27 wave 3 (lane CLONE, 2026-10-28): PARTLY (item 3 FIXED IN CODE, item 1 census added, item 2 was already fixed), PENDING HOST VERIFICATION.**
> **Item 3.** A compiled VIRTUAL `clone()` whose target is `Object.clone` now calls a thin helper from both tiers
> instead of `jit_invoke_virtual_mic`'s slow path into the native: `jit_object_clone_virtual_direct`
> (`vm/src/jit/helpers/fast_native.rs`; table slot `object_clone_virtual`, helper ABI 28), bound by
> `jit/src/clone_thin_bind.rs` at the optimizing own-site door, the spliced-body door and the single-pass door.
> Arrays of every element type (`[I`, `[J`, `[Ljava/lang/String;`, `[[I`, ... ; `[B` keeps its own helper) bind
> always; an instance site binds only when its receiver profile names a dominant class (80 %) that the VM answers
> as served (plain -- `Cloneable`, no finalizer, not `Throwable` / `LinkedHashMap` / `Properties` -- and no class on
> its chain below `Object` declares `clone()Ljava/lang/Object;`, a bridge included). The helper re-asks that per
> receiver class (memoised) and makes the site's own VIRTUAL call through `jit_invoke_dispatch` for everything it
> does not serve (null, an override, `CloneNotSupportedException`, a TLAB that cannot take the copy), so the
> answers stay the native's. Arrays: a TLAB bump of the receiver's class word, then a raw payload copy
> (primitive) or the unchecked reference arraycopy (`try_reference_arraycopy_pair`: the collector's raw-word arm
> with its post barriers, or the per-element accessors). The `byte[]` helper's copy is now that same function
> (`array_clone_serve`), and its decline descriptor became the shared virtual one (`CLONE_VIRTUAL_INFO`, was
> `BYTE_ARRAY_CLONE_INFO`; static count unchanged). Parity fix, no switch: every clone helper (`byte[]`,
> `super.clone()`, virtual) now declines to the native while a JVMTI agent listens for `VMObjectAlloc`, which the
> native posts and the helpers did not. Switch: `CRATONVM_JIT_CLONE_VIRTUAL_THIN` (default ON, `=0` restores the
> inline-cache route). Spliced bodies bind arrays only (no receiver counts there; see the proposal page
> `j27-clone-spliced-instance-sites-and-inline-expansion-proposal-20261028.md`).
> **Item 1.** New per-call census on the existing trace switch (no new switch): with
> `CRATONVM_DBG_TYPECHECK_FILTER=<substring>` (or `CRATONVM_DBG=typecheck-filter=<substring>`) every
> `jit_instanceof` / `jit_checkcast` call whose target name holds the substring prints
> `[DBG_TYPECHECK] census op=<instanceof|checkcast> site=<interned-name addr> target=<T> receiver=<id>:<R> reason=<r> answer=<a>`,
> `reason` read before the helper publishes a missing display: `array-receiver`, `displays-off`, `no-display`
> (the display / row-first arms had nothing to read), `display-verdict-0` (published, but a miss is not the
> hierarchy's alone), `display-hierarchy-only` (the arms had the facts: the site's own arm set declined it).
> Recipe: `CRATONVM_DBG_TYPECHECK_FILTER=/ cratonvm ... ZipFsFinalizerChurn 2> tc.log; grep ' census ' tc.log |
> sed 's/ site=[^ ]*//' | sort | uniq -c | sort -rn | head -40` ranks (target, receiver, reason, answer); keep
> `site=` to rank per site. Pair it with the compile-time `CRATONVM_DBG=jitc` lines (`ir instanceof display arm
> DECLINED`, `row-first arm`) for the arms each site was given. (The filter also arms the older per-stage
> `[DBG_TYPECHECK]` trace; grep `census` only.)
> **Host must confirm:** (a) `tools/probes/jit/j27/J27CloneRows.java` prints the HotSpot facts in its header under
> the default flags, `--nojit`, `CRATONVM_JIT_CLONE_VIRTUAL_THIN=0`, and each of G1 / Generational / ZGC (the churn row
> runs `System.gc()`); (b) `CRATONVM_DBG=jitc` shows `clone-virtual-bind door=ir|single-pass|splice class=[I ... Array`
> and, for `J27CloneRows$Base.dup`, `dominant=Some(..) Served`; (c) `tools/probes/spring/ObjectCloneCost.java` and the
> `jit_invoke_virtual_mic` share of `ZipFsFinalizerChurn` drop; (d) unit tests `cratonvm-jit`
> `clone_thin_bind::j27_clone_virtual_thin_tests::*` and `cratonvm-vm`
> `jit::helpers::j27_clone_copy_tests::*`, `jit::helpers::j27_typecheck_census_tests::*`, plus the
> `direct_helpers` size / row-count const asserts (200 words, 87 rows).

> **j27 wave 2 (lane TCARRAY, 2026-10-28): PARTLY (doc drift fixed; items 1 and 3 still need host evidence / a build lane), PENDING HOST VERIFICATION.** Item 2 is already in code and this page drifted: `CRATONVM_JIT_SP_SPLICE_TYPECHECK` (default ON since 2026-10-27, `cratonvm_jit::ir::sp_splice_typecheck_enabled`) admits type checks in single-pass splices (`vm/src/runtime/interpreter/jit_bridge.rs`, the `0xc0 | 0xc1` arm of the inline resolver); the `inline-resolve REFUSED ... checkcast/instanceof [tier=sp]` lines must be gone on a dev binary -- the host run confirms with `CRATONVM_DBG=jitc` on `ZipFsFinalizerChurn` (count = 0; under `=0` they come back). Item 1: one array-target slice landed in wave 2 (page `j27-typecheck-array-arm-and-array-miss-switches-are-dead-20261028.md`: `o instanceof T[]` on a plain object no longer calls `jit_instanceof`); the zipfs sites themselves are plain-class targets, so the per-site decline census of wave 1 is still the first step. Item 3 (compiled `Object.clone`): unchanged; it needs an inline allocation plus a raw copy with each collector's post barrier, not writable without a build.

> **j27 wave 1 (lane TYPECHK, 2026-10-28): NOT FIXED, PENDING HOST VERIFICATION of the census below.** No code change: each item needs host evidence first. Read-only findings to start from: item 1 -- the optimizing tier's row-first arm answers only a receiver whose row is published with status 2 (hierarchy-only); a receiver class without a published display (used before its `<clinit>` ends, or never asked by a lazy publisher) or with verdict 0 falls through the exact compare, the seen ways and the display arm to `jit_instanceof`. `IndexNode.equals` is `o instanceof IndexNode` against a class whose subclasses (`ZipFileSystem$Entry`) reach it; first step stays the per-site decline census: run the probe with `CRATONVM_DBG_JITC=1` and count `row-first arm ... DECLINED` / `display arm DECLINED: ... has no display` lines per target, then a helper-side census of `typecheck_miss_is_hierarchy_only` answers for the receivers. Item 2 -- the single-pass inliner's `checkcast/instanceof` refusal is outside this lane's files (single-pass resolver). Item 3 -- `Object.clone` of a compact instance is still the native door (`object_clone_helper` binds a thin direct call for `super.clone()` only); a compiled clone needs an inline allocation plus a raw copy with each collector's post barrier. Probes: `tools/probes/spring/ZipFsFinalizerChurn.java`, `tools/probes/spring/ObjectCloneCost.java`.

> **STATUS (2026-10-27): OPEN (perf).** Residual of
> `docs/internal/fixed-suite-bugs/spring/in-process-javac-throughput-aot-test-classes-RETIRED-20261027.md`,
> whose two VM defects (the G1 remark's reference closure, `Object.clone`'s slot-by-slot copy) are fixed.
> Nothing here is a wrong answer: every row below matches HotSpot 25.

## Where it shows

Spring's AOT / `TestCompiler` test classes run `com.sun.tools.javac` in-process over a classpath of ~1 000 jars,
which `JavacFileManager` opens through `jdk.nio.zipfs`. After the two fixes,
`AutowiredAnnotationBeanRegistrationAotContributionTests` still runs 173-220 s against HotSpot's 21-26 s (host
64.177.43.150, G1, `-Xmx4g`, load 9-17); the main thread is the critical path (~150-200 s CPU). Frame-pointer
`perf` of the main thread, bucketed by the runtime entry compiled code called into
(`tools/probes/spring/ZipFsFinalizerChurn.java` shows the same shape outside Spring):

| entry from compiled code | main thread | what it is |
|---|---|---|
| compiled Java itself | 27 % | |
| `jit_invoke_virtual_mic` | 21 % | about a third native `Object.clone` (item 3), a third interpreted callees, the rest method-handle / reflective dispatch |
| `jit_new_object` / `jit_anewarray_object` / `jit_newarray` | 28 % | mostly G1 pauses (young, remark), owned by the GC pages |
| `jit_instanceof` | 4.4 % | item 1 |
| `jit_system_arraycopy_direct`, `jit_invoke_dispatch`, `jit_vectorized_hash_code_direct` | 11 % | |

Raising `CRATONVM_TIER_C2_THRESHOLD` 3x or running one C2 thread made the main thread slower (176 s and 171 s
against 152 s), so compile volume is not the lever; the compiled code and the doors it leaves through are.

## 1. `instanceof` sites fall to the helper

`jdk/nio/zipfs/ZipFileSystem$IndexNode.equals` (`o instanceof IndexNode`, a class with subclasses),
`jdk/nio/zipfs/ZipUtils.dosToJavaTime`, `java/nio/file/FileTreeWalker.getAttributes` and
`ZipFileSystem.getFileAttributes` account for ~80 % of the `jit_instanceof` samples, all from C2 bodies whose
display / row-first arms were emitted (`CRATONVM_DBG=jitc`: `ir instanceof display arm`, `row-first arm`). The
helper's own time is object validation (`is_object_address_inner`, the live-region memo), not the hierarchy walk.
What is unknown is which run-time screen declines (no display for the receiver's class, the verdict word, the
duplicate-name latch); a per-site decline census is the first step.

## 2. The single-pass tier refuses callees that type-check

`CRATONVM_DBG=jitc` on the probe: `inline-resolve REFUSED java/lang/String.equals(Ljava/lang/Object;)Z depth=0:
checkcast/instanceof [tier=sp ...]` (21 sites), `ZipFileSystem.getInode` (15), `TimeZone.getDefault` (5),
`ZoneRules.getOffset` (5). A callee with a type check is never spliced into a single-pass body, so each of those is
a full call from the hottest zipfs paths.

## 3. `Object.clone` of an instance is a native door

`TimeZone.getDefault()` clones the default `ZoneInfo` per call, and `jdk.nio.zipfs` converts an entry time per
attribute read. After the one-validation copy (`VmHeap::copy_compact_object_fields`) a clone costs 1.3-1.9 us
(`tools/probes/spring/ObjectCloneCost.java`) against HotSpot's 60-120 ns: about half is the copy's reference slots
(each still a full `get_field` / `set_field`), half the JIT-to-native door (`jit_invoke_virtual_mic` ->
`safe_native_call`). HotSpot inlines the allocation and the copy. Still ~6 % of the main thread.

## What would close it

The class above within 3x of HotSpot's wall on an interleaved run, with items 1-3 each under 1 % of the main
thread: a decline census and the missing inline arm for item 1; a single-pass inline admission for callees whose
type checks the tier can already emit inline (item 2); a compiled `Object.clone` for a small compact instance of a
known class, or raw reference stores with each backend's post barrier in the copy (item 3).
