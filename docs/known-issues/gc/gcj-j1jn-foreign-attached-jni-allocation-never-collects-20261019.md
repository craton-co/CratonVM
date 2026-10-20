# A foreign-attached thread's refused JNI allocation never collects (Generational)

> **STATUS (2026-10-19, gcj j3/rt3): the raw-locals residual (j2-jn2 finding 9a) DESIGNED, not built.** An attachment whose locals may be held raw (`local_refs_may_be_held_raw()`) is still refused by `foreign_attached_alloc_may_collect` (`vm/src/native/jni.rs`). HotSpot collects there because its handles are indirect; CratonVM's raw local is the object address the C code may have copied into its own variables, so a MOVING collection would leave that copy stale and a non-moving one is the only sound answer. Closing it needs the ladder's collection to keep every object the attachment's open local frames name in place -- publish those entries as unrewritable conservative words of the requester for that one pause, which the Generational pinned in-place young cycle already honours -- and to refuse the compacting full rungs (`last_ditch_reclaim`, `second_major_before_oome`) while any is published. Not built blind: it touches the ladder's rungs and the pin ledger. Filed as `docs/internal/proposals/gc/gcj-j3rt3-proposal-the-foreign-ladder-pins-raw-locals-in-place-20261019.md` with the deciding rows.

> **STATUS (2026-10-19, gcj j2/jn2): residuals re-checked; the fix itself is unchanged.** The j1 build passed block FA (orch-notes) and the orchestrator flips `CRATONVM_JNI_FOREIGN_ALLOC_LADDER` this wave; nothing in the reader was edited. What stays open (not fixed, each a narrower page if it bites): (a) an attachment that may hold RAW locals (`local_refs_may_be_held_raw()`, i.e. `CRATONVM_JNI_FOREIGN_TRANSITIONS=0` or `CRATONVM_JNI_INDIRECT_LOCALS=0`) still never collects, where HotSpot does -- closing it needs the raw locals published as roots of the ladder's collection (pinned), not a rule change; (b) G1 and ZGC keep the old refusal (`foreign_attached_alloc_may_collect` tests `is_generational()`), out of the round's scope -- their owners drop the gate after their own FA rows. Re-check rows: `jn2_fa_flip_*` in `docs/internal/gc-round-gcj-20261019/j2-jn2-report.md`.

> **STATUS (2026-10-19, gcj j1/jn): FIXED IN CODE behind the opt-in `CRATONVM_JNI_FOREIGN_ALLOC_LADDER=1` (default OFF, Generational only), PENDING BUILD/PROBE.** Deciding rows: block FA of `docs/internal/gc-round-gcj-20261019/j1-jn-report.md` (`jn1_fa_*`). The page is found by reading; the positive control `jn1_fa_drop_def` must show the defect (DIFF, `FAIL oome-at=`) on the default before the switch's rows mean anything.

*Filed 2026-10-19 by gcj j1/jn, by reading.*

## What is wrong

A JNI allocation (`NewObject`, `NewObjectArray`, `New<Type>Array`, `NewStringUTF`, `AllocObject`,
`NewDirectByteBuffer`) goes through `vm/src/native/jni.rs` `jni_alloc_or_oom_sized`: one attempt on
the no-collection path, then -- only when `jni_alloc_may_collect()` -- the native reclaim ladder
(`gc_and_alloc::native_reclaim_before_alloc_retry_graced_for`: a forced young collection, the
last-ditch and second majors) and one retry, then `OutOfMemoryError: Java heap space`.

`jni_alloc_may_collect` refused every foreign-attached thread outright (`is_foreign_attached()`,
"its raw locals are not all recorded"). That rule predates the defaults of 2026-10-10: the foreign
transitions (`CRATONVM_JNI_FOREIGN_TRANSITIONS`, default ON) make an attachment a counted mutator
for the duration of every JNIEnv function (`ForeignJniEntry::enter_foreign`), and indirect locals
(default ON) leave nothing raw in the C code's variables (`local_refs_may_be_held_raw()` already says
when they do). So a host thread (`AttachCurrentThread`) that allocates and drops arrays in a loop,
with no Java thread allocating at the time, can only fill the heap: on Generational the young
refusal spills into the old generation (`GenHeap::try_alloc_array_full`, `note_young_spill_pressure`),
and once the old generation is full the allocation fails with the heap full of the attachment's own
garbage. Nothing on that thread's path collects: it never reaches the native funnel
(`vm_exec::safe_native_call_impl`, whose pressure relief and OOME debt serve Java threads), and the
boundary GC the spill pressure flags needs some other thread's boundary.

HotSpot: the JNI allocation is an ordinary VM allocation of a thread in `_thread_in_vm`, attached or
not; Serial collects (young, then full) before it throws.

## The fix (opt-in)

`jni_alloc_may_collect` asks `foreign_attached_alloc_may_collect` for an attachment: the switch on a
Generational heap (latched per VM, `JniSwitches::foreign_alloc_ladder`), the foreign transitions on,
no raw local (`!local_refs_may_be_held_raw()`; the empty-frame fallback other threads get is not
taken), and the attachment's `in_blocked_region` down right now (it is inside its JNIEnv function's
entry, a counted mutator). Then it runs the same ladder every other JNI thread runs. G1 and ZGC are
unchanged (out of the gcj round's scope); they want the same rule.

## What would close it

The FA rows: `jn1_fa_drop_def` DIFF (the defect), `jn1_fa_drop_on` / `jn1_fa_ring_on` /
`jn1_fa_ring_on_gs` / `jn1_fa_compat_on` SAME, the kill rows DIFF; then the flip (orchestrator).
If `jn1_fa_drop_def` is SAME, another door collects for the attachment: the page is refuted, and
the switch should be deleted.

Unit tests: `cargo test -p cratonvm-vm --lib gcj1jn_` (`native::jni::tests::gcj1jn_a_counted_foreign_attachment_may_collect_on_generational_with_the_switch`,
`gcj1jn_a_foreign_attachment_never_collects_by_default_or_on_g1`,
`gcj1jn_the_three_opt_ins_are_latched_per_vm_and_off_by_default`).
