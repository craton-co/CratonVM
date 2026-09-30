# A JNI local ref is a raw heap address: the native's own copy goes stale across any moving collection it lets run

> **STATUS (2026-09-29, gce e1/x): KEEP -- unchanged; retires with the JNI package flip, which is held by its cost.**
> - Evidence (`verify-e1/ve1`, e1 vs base `adb9178bc`, identical on both): default JNI `*_jniroots_A_1..2` and `*_jniblock_A_1..2` FAIL on Generational, G1 and ZGC (raw locals, this page); with the package `*_jniroots_P_1..2`, `*_jniroots_P3_1..3` (audited), `*_jniroots_E_1..2` and `*_jniblock_P*` print HotSpot's lines on all three collectors. Package `noop` still costs about 6 us against 0.8 us for the default (orchestrator; unchanged by e1), bar 2x.
> - **Remaining:** the package cost (follow-up e1b halved it on Generational but regressed the default arm on G1/ZGC), then the flip.

> **STATUS (2026-09-28, gcd d10/j, lane jni10; by reading, no cargo): OPEN
> with the defaults; the fix is now ONE opt-in package, and the unsafe arm C
> can no longer be selected.**
>
> - `CRATONVM_JNI_NATIVE_TRANSITIONS=1` alone turns on this page's
>   `CRATONVM_JNI_INDIRECT_LOCALS` together with both transition flags
>   (`vm/src/native/jni.rs::jni_switches`); `CRATONVM_JNI_INDIRECT_LOCALS=1`
>   alone still works (arm B), and its explicit `0` still wins over the
>   package.
> - Arm C (`CRATONVM_JNI_FOREIGN_TRANSITIONS=1` without indirect locals, the
>   crash below) is REFUSED: the foreign transitions stay off, one
>   `[cratonvm] CRATONVM_JNI_FOREIGN_TRANSITIONS is ignored ...` line goes to
>   stderr, and the run behaves as arm A. Unit test:
>   `cargo test -j 5 -p cratonvm-vm --lib jni_switches_move_as_one_package_and_refuse_arm_c`.
> - Expected, each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`,
>   `-XX:+UseZGC`, 3 runs, `Gcd1JniRootsProbe` (commands in its class
>   comment): arms D, E and `P="CRATONVM_JNI_NATIVE_TRANSITIONS=1"` print
>   HotSpot's seven lines ending `PASS all 6`, rc 0; arm C prints arm A's
>   lines (foreign lines FAIL), rc 1, never 139.
> - Remaining gate: the package's cost bar (`noop`), then items 1-3 of
>   `gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md` (its d10/j
>   STATUS). With the defaults this page's defect is unchanged.
>
> *Previous (d8/x, wave d7, Generational):*
>
> - **Correctness, d7:** `Gcd1JniRootsProbe` arm D (indirect locals + foreign transitions) and arm E (all three) print HotSpot's seven lines 3/3 each (`jniroots_D_1..3`, `jniroots_E_1..3`), arm E with the roster audit 3/3 with 0 `ROSTER HOLE` (`jniroots_E_audit_1..3`); arms A (defaults) and B (indirect locals only) fail the foreign lines 3/3 as designed.
> - **New evidence of this page's defect:** arm C (`CRATONVM_JNI_FOREIGN_TRANSITIONS=1` alone, i.e. an attached thread's locals still RAW) crashed once in three (`jniroots_C_1`, rc 139): SIGSEGV inside the VM on an indexed load at an address in a heap span the collector had decommitted (`site=unbumped-middle`), right after `local-monitor-across-upcall: PASS`. That is a raw local read after a moving collection, exactly this page; arm C is not a supported combination (the three flags flip together), and the two other arm-C runs fail `foreign-idle-locals` / `foreign-call-result` / `foreign-array-elements` without a crash.
> - **Cost, `Gcd1JniCostProbe`, medians of the five per-run medians (ns/call; `jnicost_A/B/E_1..5`):** arm A `noop 526`, `array-length 17`, `int-region 25`, `new-string 502`; arm B `535`, `17`, `26`, `493` (B/A ~1.0: indirection itself is free); arm E `4797`, `25`, `34`, `9097`. E/B is 9.0x on `noop` (bar 2x) and 18x on `new-string` (bar 5x); the leaf functions are inside the bar (1.3-1.5x).
> - **Remaining gate:** the per-bracket entry deposit (`gcd-d5f-proposal-light-in-native-deposit-and-incremental-reentry-20260928.md`) to bring arm E under the bar, then items 1 (G1 / ZGC), 2 and 3 (real-workload JNI census) of the counted-mutators page.

> **STATUS (2026-09-28, gcd d5/f): PARTIALLY FIXED, flag still default OFF;
> gate unchanged (one gate for the three JNI flags, on
> `gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`).** What
> changed around this flag: the transition cost that made the package
> unflippable is cut on the hot JNIEnv leaf functions (leaf windows,
> `jni.rs::ForeignJniEntry::enter_leaf`) and on every in-native bracket's
> leave (quiet leave), both only when this flag and
> `CRATONVM_JNI_NATIVE_TRANSITIONS` are on. Leaf windows are taken ONLY for a
> handle that decodes as this thread's indirect local (`decode_indirect_local`);
> a raw local, a global ref or a `jclass` takes the full transition, so this
> flag's escape accounting (`raw_local_escapes`) is untouched. Arm B vs A of
> `Gcd1JniCostProbe` (the cost of indirection alone) is unaffected by d5/f.

> **Previous STATUS (2026-09-28, gcd d4/k): PARTIALLY FIXED, flag still default OFF;
> the flip gate is now ONE gate for three flags.** First Linux run of the d3
> build (Generational, run 1): `Gcd1JniRootsProbe` arm E (all three JNI flags)
> printed HotSpot's seven lines, arms A and B `rc=1` (arm B fails the
> foreign lines by design: an attached thread's locals stay raw without the
> foreign transitions). Runs 2-3 and G1/ZGC pending.
>
> **What still has to pass before this flag is the default** (written out on
> `gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`'s d4/k
> STATUS, items 1-4, which this page shares): `CRATONVM_JNI_INDIRECT_LOCALS`,
> `CRATONVM_JNI_NATIVE_TRANSITIONS` and `CRATONVM_JNI_FOREIGN_TRANSITIONS` flip
> together or not at all -- this one first is also acceptable, never after the
> native transitions. For THIS flag specifically:
>
> * the per-call cost of indirection is arm B against arm A of the new
>   `tools/bench/Gcd1JniCostProbe.java` (medians of the `array-length`,
>   `int-region`, `new-string` cases; each decode of an indirect local is one
>   thread-local frame lookup, so B/A should be close to 1);
> * the behaviour JNI libraries can see (a local used on another thread, kept
>   past its native's return, or smuggled through a `jlong`) is what the
>   real-workload census and flipped runs of that page's item 3 look for
>   (Netty native transports + netty-tcnative first; Tomcat and Spring Boot
>   as the zero-JNI controls);
> * the strict JNI corpus with the three flags (as before).
>
> Design re-audit for escapes (d4/k): every producer of a local handed to
> native code goes through `record_local_handle_indirect` (raw fallbacks
> counted per thread), `ExceptionOccurred`'s raw pending-handle fallback is
> now counted too, `NewObjectA` and `PopLocalFrame` keep the indirect form.
> New for lane m's pinned young copy: `jni::for_each_live_local_ref` and
> `jni::local_refs_may_be_held_raw` (read-only, `pub(crate)`): with this flag
> off the answer is "held raw", so a thread's locals must be pinned, not
> moved, by a copy that cannot rewrite them.
>
> **Previous STATUS (2026-09-27, gcd d3/k): PARTIALLY FIXED, flag still default OFF
> (not flipped here); no code change to the indirection itself. The flip gate
> of d2/i below stands.** What changed around it: this flag is now the
> precondition of `CRATONVM_JNI_NATIVE_TRANSITIONS`
> (`gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md`), which runs a
> JNI native GC-safe ("in native") and so lets ANY thread's moving collection
> run while the native holds its locals. That is sound only because the
> native's handles are slots the leave rewrites; with this flag off the
> native transitions stay inert. Two small additions in `vm/src/native/jni.rs`:
> the flag-and-attachment rule is one function, `locals_are_indirect_here`
> (used by `record_local_handle_indirect` and by the native transitions), and
> a handle that goes out RAW although that rule held (no open frame, a slot
> past the encodable range) is counted per thread (`raw_local_escapes`) so the
> native transitions keep that call counted. When the flags flip, flip this
> one first or together with the native transitions, never after.
>
> **Previous STATUS (2026-09-27, gcd d2/i): PARTIALLY FIXED, flag still default OFF
> (not flipped here). The cheapest out-of-process run is now a probe:
> `tools/bench/Gcd1JniRootsProbe.java` + `tools/probes/jni/Gcd1JniRootsProbe.c`.
> No code change under this page.**
>
> Its two same-thread lines are this page's runtime confirmation, written as
> the "Confirmation" section below asks: `local-across-upcall` holds a local
> across an allocating up-call and asks `IsSameObject(local, global)` plus a
> field read (a stale raw copy answers `FAIL code=1`, never a crash);
> `local-monitor-across-upcall` is the sharpest consequence named below --
> `MonitorEnter(o)`, a moving up-call, `MonitorExit(o)` through the same
> local, then a daemon contender that must get the monitor (`FAIL code=1
> released=false` is the hang, bounded). Build, the HotSpot oracle and the
> four-arm matrix are on
> `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md`'s STATUS.
> For THIS flag the gate is arm B (`CRATONVM_JNI_INDIRECT_LOCALS=1` alone):
> `local-across-upcall: PASS` and `local-monitor-across-upcall: PASS`, 3/3 on
> each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`, and arm D
> (both flags) HotSpot's seven lines 3/3. Arm A (defaults) failing either
> line at least once in the nine runs is the evidence the flag fixes
> something; A passing 9/9 means no collection moved the object inside the
> window (then raise the probe's churn, not the verdict). Steps 1-3 below
> (strict JNI corpus, netty-tcnative / JNA / lz4 / zstd, attached threads)
> still stand before the flip.
>
> **Previous STATUS (2026-09-26, at `b5c9b6c6e`): PARTIALLY FIXED. The indirection
> has landed behind `CRATONVM_JNI_INDIRECT_LOCALS`, default still OFF.
> Nothing in-process is left; what blocks the default flip is two
> out-of-process runs nobody has recorded.** No code under this page has
> changed since gc-common w10-c (`git log -S CRATONVM_JNI_INDIRECT_LOCALS
> -- vm/src` stops at `4f91ca7ee`, w7-c).
>
> **What landed (all in `vm/src/native/jni.rs`).**
>
> * The indirection, flag-gated (w6-g, `79333d628`). `indirect_locals_active`
>   (`:3242`) reads `CRATONVM_JNI_INDIRECT_LOCALS` (grouped token
>   `CRATONVM_GC=jni-indirect-locals`, `types/src/flag_groups.rs:977`); the
>   value rule `indirect_locals_from` (`:3259`) treats unset as OFF. With the
>   flag on, a local is `0x7F52 << 48 | depth << 32 | slot << 1`
>   (`encode_indirect_local`, `:3285`), a slot in `JNI_LOCAL_FRAMES`, which
>   `collect_local_ref_roots` already roots and `update_local_refs_after_gc`
>   already rewrites. Every producer goes through `new_local_handle`
>   (`:3187`) / `record_local_handle_indirect` (`:3323`); every consumer
>   through `jobject_to_obj` (`:3409`). `DeleteLocalRef` clears and reuses
>   the slot; `PopLocalFrame` re-creates an indirect result in the parent.
>   With the flag off every handle is the raw address, as before.
> * Handles stay raw even with the flag on in three cases: (a) no open frame;
>   (b) a foreign-attached thread, unless `CRATONVM_JNI_FOREIGN_TRANSITIONS`
>   is also on (w10-c: the rule is `indirect_locals_active() &&
>   (!is_foreign_attached() || foreign_transitions_active())`, because the
>   attach-level frame then lives until detach; see
>   `common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked.md`);
>   (c) a frame deeper than `INDIRECT_LOCAL_MAX_DEPTH` (`:3236`), by design.
> * Liveness holes closed on the way, all default-on, flag on or off:
>   every JNI function that hands out a local now records it in the innermost
>   open frame (w3-g); `NewObjectA` returns the slot's post-`<init>` value
>   (w3-g); `PopLocalFrame` roots its result in the parent and never pops a
>   VM-opened frame (w4-g, w7-c); `DeleteLocalRef` removes one entry, not
>   every equal one (w4-g); `ExceptionOccurred` hands out a recorded local
>   (w4-g); `NewStringUTF` / `NewString` no longer intern (w7-c); the
>   dispatch's implicit frame truncates to its entry depth
>   (`JniImplicitFrameGuard`, `vm/src/vm/vm_exec.rs:35669`,
>   `applied/handoff-w7c-implicit-jni-frame-closes-leaked-frames.md`).
>
> **Tests (`jni.rs`, `mod tests`).** `indirect_local_flag_rule_and_encoding`
> (`:12361`), `indirect_local_handles_follow_a_moving_collection` (`:12414`),
> `indirect_locals_follow_a_real_{generational,g1,zgc}_collection`
> (`:12700`ff: a REAL `VmHeap::collect_garbage` plus a forced relocation,
> covering the object, array, string, frame, exception, global-ref,
> identity and monitor functions),
> `indirect_locals_array_copies_follow_a_real_*_collection` (`:12861`ff: the
> `Get/Release*Elements`, `*Critical` and region copies),
> `indirect_locals_through_constructors_fields_and_calls` (`:13252`),
> `indirect_locals_stay_raw_where_a_slot_cannot_name_them` (`:12885`),
> `idle_foreign_jni_objects_survive_a_*_collection` (`:15898`ff, both flags
> on), and the liveness tests `new_local_handle_is_rooted_only_inside_an_open_frame`,
> `pop_promotes_the_result_and_delete_removes_one_equal_ref`,
> `exception_occurred_hands_out_a_rooted_local_ref`,
> `pop_local_frame_never_pops_a_vm_frame_and_scope_exit_closes_leaks`,
> `jni_strings_are_new_and_not_interned`. Not re-run for this update.
>
> **Still open.** With the default OFF, a native's own copy of a local is
> still a raw address a moving collection does not rewrite
> (`obj_to_jobject`, `:3154`, is still `obj.as_ptr() as u64`). The sharpest
> consequence is a hang, not a wrong value: a contended `MonitorEnter(o)`
> blocks in `monitor_enter_blocking`, a moving collection moves `o` and the
> monitor table follows it, and the native's later `MonitorExit(o)` names the
> from-space address, so the moved object's monitor is never released.
>
> The flip is blocked on third-party native behaviour the in-process tests
> cannot cover (a local used on another thread, kept past its native's
> return, or smuggled through a `jlong` -- all undefined behaviour under the
> JNI spec, all "working" with raw handles until an object moves). Owner:
> whoever runs the JNI workloads (Linux host). In order:
>
> 1. The strict JNI corpus fixture with `CRATONVM_JNI_INDIRECT_LOCALS=1`
>    (`docs/internal/gc-common-round-20260923/w6-g-report.md` section 7,
>    probe 3). Cheapest; do it first.
> 2. The JNI-library run -- netty-tcnative `ParameterizedSslHandlerTest`,
>    JNA `libjnidispatch`, lz4-java / zstd-jni -- with the flag on, and the
>    same run with the flag off plus `CRATONVM_DBG_JNI_LOCALREF=1
>    CRATONVM_DBG=gcpart` (any report there is a stale raw local the flag
>    would fix).
> 3. For attached host threads, the same run with
>    `CRATONVM_JNI_FOREIGN_TRANSITIONS=1` as well.
>
> When 1 and 2 are clean: flip `indirect_locals_from`'s unset case to ON
> (keep `0`/`false`/`off`/`no` as the kill switch), make the
> `jni-indirect-locals` flag-group row default-on, and move this page to
> `docs/internal/gc-common-round-20260923/` as FIXED.

Status: PARTIALLY FIXED (flag default OFF)
Area: `vm/src/native/jni.rs` (`obj_to_jobject`, `jobject_to_obj`, `JNI_LOCAL_FRAMES`)
Filed: 2026-09-23, gc-common round, wave 2, lane C
Backends: all three (every moving collection: Generational young copy, G1 evacuation, ZGC slide)

## Evidence

- `jni.rs::obj_to_jobject` returns `obj.as_ptr() as u64`; `jobject_to_obj`
  treats a bit-0-clear handle as "a raw heap pointer", validated by
  `is_heap_addr` (alignment + region containment -- a vacated or recycled
  address passes).
- The VM's own record of the local refs IS a GC root and IS remapped: the
  per-thread `JNI_LOCAL_FRAMES` stack is collected by `collect_local_ref_roots`
  (initiator, `roots.rs` 9b; blocked threads, `deposit_root_snapshot`) and
  rewritten by `update_local_refs_after_gc` / the blocked-wake fixup. But native
  code does not read that table: it holds the `jobject` VALUE it was handed, in
  its own registers, stack and structs, and nothing can rewrite those.
- Global refs do not have the problem: they are tagged indices into
  `JniGlobalRefs`, resolved at every use.

## Failure scenario

Native code receives `jobject o` (a local ref), then does anything that lets a
moving collection run before its next use of `o`:

- an up-call (`Call<Type>Method`, `NewObject`, ...) that allocates and triggers
  a young collection on this thread;
- a contended `MonitorEnter` -- since w2-c a counted caller takes the canonical
  GC-safe acquire (`monitor_enter_blocking`: deposit, block, remap), so a pause
  may now run while it sleeps instead of DEADLOCKING the VM as it did before
  (`common-a-jni-placeholder-thread-id-at-barrier`); the table is remapped on
  wake, the native's copy of `o` is not;
- a foreign-attached thread between its calls (GC-blocked by design).

After the move, `o` is the from-space address. `jobject_to_obj(o)` still
accepts it if the address is inside a live region, and the native reads or
writes a vacated or recycled object -- silently, at an arbitrary later site.

## Proposed fix

Make local refs indirect, like global refs: hand out
`(frame_index << k) | (slot << 2) | TAG_LOCAL` into `JNI_LOCAL_FRAMES`, resolve
at every use (the table is already remapped by every collection), and keep
`obj_to_jobject`'s raw form only for the two places that need an address
(`GetDirectBufferAddress`-style paths do not take jobjects). `DeleteLocalRef`,
`PopLocalFrame` and `NewLocalRef` already operate on the table. Cost: one TLS
lookup per `jobject_to_obj` on a local ref, which the global-ref path already
pays with a mutex.

## Confirmation

```
rg -n "pub fn obj_to_jobject" -A 3 vm/src/native/jni.rs
```

still shows `obj.as_ptr() as u64`. Runtime: a JNI test library that takes a
local ref, calls back into Java to allocate until a young collection runs,
then reads a field through the original `jobject` under `-XX:+UseGenerationalGC`
with `CRATONVM_DBG_STALE_OBJREF=1`.

## What would retire it

Local refs resolved through the remapped table at every use.
