# Allocation-driven collections do not clear weakly-reachable referents; only `System.gc()` does

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): cause 1 FIXED and re-measured; the page stays open for cause 2 (G1) only.** `WeakClearYoungProbe` and `WeakClearNoLoadProbe`, `-XX:+UseGenerationalGC -Xmx256m`, print `inline=cleared viaMethod=cleared allocations=<n> PROBE-OK` in the default mode (`w4o_wcy_gen`, `w4o_wcnl_gen`) and in `--compatible` (`w4o_wcy_gen_compat`, `w4o_wcnl_gen_compat`). The ZGC rows of the list were not in the d7 battery. Remaining: cause 2 in `G1Collector::pinned_region_set_including_non_object_roots` (G1 owner); retire when `WeakClearNoLoadProbe` prints `PROBE-OK` on `-XX:+UseG1GC`.

> **STATUS (2026-09-28, gcd d5/r): cause 1 is FIXED AND WAS MEASURED (the
> "pending the probe run" below is stale); only cause 2 (G1, out of this
> round's scope) keeps the page open.**
>
> * **Cause 1's probe run exists.** `docs/internal/gc-common-round-20260923/orchestrator-w36-verification.md`
>   ("Probes", confirmed fixed): "`WeakClearYoungProbe` passes on
>   Generational and ZGC (with F36)" on the wave-36 integration build, all
>   three backends, both modes; the same file lists it "still failing, as
>   expected" on G1 only (cause 2). The probe is
>   `tools/probes/WeakClearYoungProbe.java` (not `tools/bench`).
> * **Nothing since has touched cause 1's fix** (the eleven JIT wrappers'
>   `drop_native_return_handed_to_compiled_code`, `vm/src/jit/helpers.rs`),
>   and the probe runs no `System.gc()`, so this round's stop-the-world
>   changes (d4/n's true-root seed is for requested majors only) do not reach
>   it. Re-confirm on the round build if wanted (3/3, JIT on, and once with
>   `--nojit`):
>   ```
>   javac -d /tmp/wcy tools/probes/WeakClearYoungProbe.java
>   java -XX:+UseSerialGC -Xmx256m -cp /tmp/wcy WeakClearYoungProbe
>   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp /tmp/wcy WeakClearYoungProbe
>   ```
>   Expected: `inline=cleared viaMethod=cleared allocations=<n> PROBE-OK` on
>   both (`<n>` differs by collector and run; compare the other tokens).
> * **Split:** the Generational/ZGC half (cause 1) can be recorded as fixed
>   now; the page's remaining owner is the G1 collector (cause 2,
>   `G1Collector::pinned_region_set_including_non_object_roots`). Retire the
>   page when `WeakClearNoLoadProbe` prints `PROBE-OK` on G1.
>
> **STATUS (2026-09-26, gc-common w36-f, superseded above): PARTLY FIXED. Cause 1 is fixed
> and confirmed: on the wave-36 integration build `WeakClearYoungProbe`
> (`-Xmx256m`) prints `inline=cleared viaMethod=cleared ... PROBE-OK` on
> Generational and ZGC in both modes (it printed `inline=LIVE` before), and
> `WeakClearNoLoadProbe` passes on all three. Cause 2 is unchanged and
> G1-internal (owner: the G1 collector, `gc/src/g1.rs`): G1 still prints
> `inline=LIVE viaMethod=LIVE`.**
>
> **Cause 1 fix (w36-f, applying
> `docs/internal/gc-common-round-20260923/applied/handoff-w36b-jit-fast-native-return-stays-rooted.md`,
> option 1).** Every JIT helper that can return a native's object to
> compiled code now drops `native_pending_return` in its extern wrapper,
> after the body under its panic guard has finished and so after the
> helper's last GC point: `jit_invoke_dispatch`, `jit_invoke_virtual_mic`,
> `jit_indy_bridge`, `jit_varhandle_read_direct`, and the direct helpers for
> `Integer.valueOf`, `Long.valueOf`, `Thread.currentThread`,
> `ConcurrentHashMap.get`, `HashMap.get`/`put` and `StringLatin1.toLowerCase`
> (`vm/src/jit/helpers.rs::native_return_handed_to_compiled_code` /
> `drop_native_return_handed_to_compiled_code`). The two Rust callers that
> keep such a result across a later GC point pin it instead
> (`varhandle_strict_reference_return_check`, `lambda_int_to_double_generic`).
> Tests: `helpers.rs::w36f_native_return_handoff_tests` (the currentThread
> direct helper leaves the slot empty; a source witness over the eleven
> wrappers). On by default, no flag: the argument is in the doc of
> `drop_native_return_handed_to_compiled_code` and in `w36-f-report.md`.
> Measured before the fix on `cratonvm-gccommon-w36`: `WeakClearYoungProbe`
> on ZGC `-Xmx256m` prints `inline=LIVE viaMethod=cleared ... PROBE-FAIL`.
> **Retires** when it prints `PROBE-OK` on Generational and ZGC (G1 then
> fails only on cause 2).
>
> **The w36-b diagnosis (still correct): cause 1 is not a frame word; it is
> `native_pending_return`.** Measured
> on the pre-w36 binary (`target/release/cratonvm.exe`, 2026-09-25):
> `CRATONVM_JIT_DENY=WeakClearYoungProbe.main` clears on all three
> collectors, so the retention comes with compiling `main`; but turning off
> each frame channel (`CRATONVM_DBG_NO_JIT_ROOT_SCAN=1`,
> `CRATONVM_JIT_SAFEPOINT_REG_SPILL=nostore`, `CRATONVM_GC_REG_OOP_MAPS=0`,
> `CRATONVM_NO_MOVING_YOUNG=1`) keeps it LIVE, keeping `main`'s dead
> `String[] a` live changes nothing, and a compiled Java call returning
> `int` between the load and the allocation changes nothing. One NATIVE
> call there -- `Thread.currentThread()` or `System.identityHashCode` --
> clears it on Generational and ZGC. `WeakReference.get` is a native bridge
> (`native_ref_get`); from compiled code its result is left in the thread's
> `native_pending_return` handoff root (rooted by `memory/roots.rs:497`),
> which only the next native call resets. The interpreter consumes that root
> when it pushes the value; compiled code never did. The edit (clear it once
> the helper hands the object to compiled code) is applied, see above.
>
> **Reference homes (the brief's cause 1, done in w36-b).** The single-pass
> tier no longer publishes a dead reference local whose register another
> local shares (`jit/src/x64/safepoint.rs::dead_shared_register_oop_locals`;
> `common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive-FIXED-20260926`).
> That is the liveness the JIT has at a safepoint, applied where the home was
> wrong. A dead reference that still owns its home (a frame-homed local, or a
> register only it uses) is still published: it can only retain, and no
> probe here needs it gone. The IR tier publishes every defined `Ref` node's
> slot until the method returns (`ir_lower.rs::emit_safepoint_map`), the
> same retention class, by design (its "Reference / primitive separation"
> note). Neither is what `WeakClearYoungProbe` measures.
>
> **Retire split (unchanged).** Cause 1 retires when `WeakClearYoungProbe`
> prints `PROBE-OK` on Generational and ZGC (expected now that the handoff is
> applied); cause 2 when `WeakClearNoLoadProbe` prints `PROBE-OK` on G1.
>
> The pre-w36 STATUS follows (its cause 1 bullet is superseded by the
> above; the cause 2 analysis stands):
>
> * Cause 2 is unchanged: `G1Collector::pinned_region_set_including_non_object_roots`
>   (`gc/src/g1.rs:25275`) still excludes a region holding a conservative
>   JIT root from the collection set and walks it wholesale, and
>   `VmHeap::watched_pre_gc_addr_survived` (`gc/src/vm_heap.rs:5868`) still
>   answers G1 through the region-granular `is_addr_live` /
>   `G1Collector::is_addr_in_live_region` (`g1.rs:26048`). The one later G1
>   pinning change, `2341fc6f6` ("pin an interior root's region instead of
>   evacuating it"), adds pins rather than tracing inside them.
>
> The w5-d measurement follows, on the round's w4 binary
> (`cratonvm-gccommon-w4`) with `-Xmx256m`.
>
> | run | Generational | G1 | ZGC | HotSpot |
> |---|---|---|---|---|
> | `WeakClearYoungProbe` | inline LIVE | both LIVE | inline LIVE | OK, about 173k allocations |
> | same, `CRATONVM_DISABLE_JIT=1` | **OK**, about 118k | **OK**, about 733k | **OK**, about 737k | |
> | `WeakClearNoLoadProbe` (new, `refersTo(null)`) | **OK**, about 120k | both LIVE | **OK**, about 737k | OK, about 177k |
> | same, `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` (unsound, diagnostic only) | | **OK**, about 733k | | |
>
> With the JIT off, every backend clears both referents on allocation-driven
> pauses, which are `Pause Young (Allocation Failure)` under `-Xlog:gc`.
> `CRATONVM_DBG=weakref` shows the pre-collection null pass and the restore
> pass running at every such pause. So allocation-driven collections do run
> reference discovery, and they do clear. What keeps each referent is a root.
>
> 1. **(w5-d's reading, SUPERSEDED by w36-b above: the root is the
>    `native_pending_return` handoff, not a frame slot.)**
>    **Dead JIT temporaries are roots. JIT round; all three collectors;
>    the `inline` column.** The loop `while (r[0].get() != null || …)` loads
>    the referent on every iteration. The result is dead after the null
>    test, but the compiled frame keeps it in a slot that the root scan
>    reads, so the referent is strongly reachable at every pause. HotSpot's
>    liveness-precise oop maps drop it.
>    * The evidence: the variant that never loads the referent
>      (`refersTo(null)`, `tools/probes/WeakClearNoLoadProbe.java`) clears
>      on Generational and ZGC.
>    * `inline` stays LIVE with `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1`, so the
>      slot is not in the conservative native-stack band. It is inferred,
>      not traced, to be a precisely scanned JIT home (shadow stack or
>      reference spill).
>    * It is the referent loaded LAST that is kept, not the one created
>      "inline". Swapping creation order, or creating both in a callee that
>      has returned, keeps `r[0]`, the one the `||` loads every iteration.
>    * The common idiom `while (ref.get() != null) allocate();` therefore
>      loops until its bound whenever the loop is compiled.
>    * Fix: the JIT publishes a reference home only while the value is
>      live, i.e. liveness-aware oop maps or clearing dead homes at
>      safepoints. The code is in `jit/` and `vm/src/jit/`, outside this
>      round.
> 2. **G1 pins a whole region per conservative JIT root. G1 round; the
>    `viaMethod` column on G1.** `CRATONVM_G1_DBG_PINS=1` prints, at each of
>    the three young pauses, `pin_regions={15, …}` with region 15
>    `Eden occ=1023K/1024K pins=2 jit`. That is the region `setup()`
>    allocated the two `WeakReference`s, their referents and the array into.
>    * Mechanism: `G1Collector::pinned_region_set_including_non_object_roots`
>      (`gc/src/g1.rs`, the young pause and the parallel young pause)
>      excludes a region holding any conservatively found JIT root from the
>      collection set. It then walks that region wholesale as a source, so
>      every object in it survives, reachable or not.
>    * The survival verdict for the dead referent is `VmHeap::is_addr_live`'s
>      G1 arm, `is_addr_in_live_region`, called through
>      `watched_pre_gc_addr_survived` (`gc/src/vm_heap.rs`). It is
>      region-granular and answers "live". The restore pass then writes the
>      referent back.
>    * Given the pin, that verdict is the only sound one. No object in a
>      pinned region is marked, so a referent reachable from a pinned
>      neighbour is indistinguishable from a dead one.
>    * The pin is re-established at every pause, because the live array
>      never leaves the region. Region 15 therefore stays Eden, is never
>      collected, and its roughly 1 MiB of garbage is retained along with
>      the referents. Only a concurrent mark or a full GC can clear them.
>    * Fix: trace precisely inside JIT-pinned regions, marking the objects
>      reached from roots and CSet survivors and emitting identity
>      `pointer_map` entries for them. The G1 arm of
>      `watched_pre_gc_addr_survived` can then answer per object for an
>      address in a pinned young region. This is collector-internal
>      (`g1.rs`, `vm_heap.rs`).
>
> A small correction to the table below: on ZGC `viaMethod` IS cleared, and
> only `inline` stays LIVE (cause 1 alone). Only G1 keeps both, because it
> has both causes.
>
> (Retire split: as at the top of this block.)

**Status: OPEN (cause 1: fixed by w36-f, `applied/handoff-w36b-jit-fast-native-return-stays-rooted`, pending the probe run; cause 2: G1 collector).** Filed 2026-09-24 by the gc-common round orchestrator during
the wave-4 verification. It predates the round: the `w0` binary behaves the
same. Probe: `tools/probes/WeakClearYoungProbe.java`.

## Symptom

```java
WeakReference<Object> inline = new WeakReference<>(new Object());   // in main
WeakReference<Object> viaMethod = make();                            // referent created in a callee
while ((inline.get() != null || viaMethod.get() != null) && n < 5M) sink = new byte[256];
```

| | `inline` | `viaMethod` | after `System.gc()` |
|---|---|---|---|
| HotSpot (Serial, G1, ZGC) | cleared after ~173k allocations | cleared | cleared |
| CratonVM Generational | LIVE after 5M allocations | cleared | cleared |
| CratonVM G1 | LIVE | LIVE | cleared |
| CratonVM ZGC | LIVE | LIVE | cleared |

5M × 272 B is ~1.3 GB of allocation, so there are many young collections.

## Why it matters

A `WeakHashMap`, a cache keyed by `WeakReference`, or a `ClassValue` /
`ThreadLocal` table releases entries only on an explicit `System.gc()`. That
is a memory-retention divergence, not only a timing one: without a forced GC
the referent is never released. A program that waits on a
`ReferenceQueue` for a weak reference to be enqueued, with no `System.gc()`,
never gets it on G1/ZGC.

## Candidates (as filed; superseded by the w5-d diagnosis above)

* G1 / ZGC allocation-triggered (young) collections may not run reference
  discovery for `WeakReference` at all, or treat every referent as a root.
  `viaMethod` has no stale stack slot anywhere, yet it stays LIVE. Check
  what the young/allocation door passes to reference processing
  (`process_references_after_gc`, `g1_remark_process_references`, ZGC's young
  cycle).
* `inline` on Generational may be held by a stale compiled-frame or
  conservative slot of `main`, the argument of the `WeakReference.<init>`
  call. That is legitimate conservatism, but HotSpot's precise maps do not keep it.

## Retire when

`WeakClearYoungProbe` prints `PROBE-OK` on all three collectors without a
`System.gc()`, or the page records why a collector legitimately defers
weak-reference clearing to a full cycle and the divergence is documented in
`docs/GC.md`.
