# The A5 cycle ran the non-moving sweep without the pass that makes it safe — FIXED

| | |
|---|---|
| **Status** | **FIXED 2026-09-08.** The root-coverage gap the page named as its "most specific lead" was real, and is closed. See *What is and is not proven* — the Windows reproduction was not re-run. |
| **Scope** | `--XX:UseGc Generational`, JIT on, `CRATONVM_DBG_GC_STRESS=4194304`. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests` (Windows; 26 of 27 tests passed) |
| **Victim** | `net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType` |
| **Fix** | `vm::memory::roots::collect_roots` step **14a5** — the conservative frame pass, run after the JIT scan instead of before it |

## The report

```text
ERROR cratonvm::gc::guard: receiver is inside a YOUNG span the non-moving sweep
zeroed and returned to the free list.
  obj="0x11c5ee9b988"  site="invoke dispatch"  actual_class_id=0
  target_class=java/lang/Object.asGenericType()Lnet/bytebuddy/description/type/TypeDescription$Generic;
  freed_span="0x11c5ee9b988+0x28"  interior_off=0
  sweep_cycle=6322  free_seq=16909
  root_coverage="NEVER-LOOKED"  xt_passes=0 xt_taken_over=0 xt_unclassified=0

ERROR cratonvm::gc::guard: …and it was RECLAIMED BY THE YOUNG SWEEP while still
reachable. The original class names the root-coverage gap.
  original_class=net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType
```

## The defect

Two facts in that report fix the collector state exactly:

* the **non-moving young sweep** ran (`freed_span`, `sweep_cycle`);
* `xt_passes=0` — the cross-thread take-over never scanned a peer, because it
  is gated on `any_thread_in_jit()`. Single-threaded, nobody in compiled code.

The generational collector runs the non-moving sweep for either of two reasons
(`gen_heap::collect_garbage_inner`):

```rust
let has_conservative_roots = gc_quiescence::is_active()
    || gc_quiescence::unregistered_jit_frame_on_stack();
```

With no thread inside a `JitEntryGuard`, the first is false. So this cycle took
the sweep on the **A5** term: a compiled frame live on the native stack without
a guard, found by `conservative_roots::scan_active_jit_frames`, which
conservatively marks that frame's band and sets the flag.

And this is what step 1 of the root scan asked at the time:

```rust
pub(crate) fn conservative_locals_enabled() -> bool {
    base && cratonvm_gc::gc_quiescence::is_active()   // ← the FIRST reason only
}
```

`conservative_locals` is what suppresses the per-bci local-liveness filter
(`scan_local_objects_all_live`) and what runs the tag-independent probes
(`scan_locals_conservative`, `scan_object_refs_conservative`) that recover a
reference from a frame slot whose `CompactValue` object tag was lost. Its own
doc gives the reason it is safe: "the only mode in which a false-positive root
is harmless (nothing is relocated)". That is equally true on the A5 path — it
is *why* the collector forces the sweep there — **and the pass did not engage
on it.** The sweep freed on `GC_FLAG_MARKED` with the root-widening pass off.

### Why a lost-tag slot is the shape that fits

The terminal is an interpreter **INVOKE**, and the receiver is
`TypeDescription$Generic`, i.e. the value `TypeDescription.ForLoadedType.of(t)`
returned — a JIT callee's object return value sitting on the interpreter's
operand stack when the sweep ran. `roots.rs`' own step-1 comment describes the
channel: "A JIT callee's object return value can reach an interpreter local
under a non-object tag … the tag-filtered `scan_local_objects` then omits it".
`ValueStack::scan_object_refs` applies the same `kinds` filter to the operand
stack, and `scan_object_refs_conservative` is the backstop — gated on the same
predicate. An unmarked slot on a sweep cycle is a zeroed object, and the next
instruction reads `class_id == 0`, resolves against `java.lang.Object`, and
reports `java/lang/Object.asGenericType()`. That is the report, term for term.

## The three predicates — one of them was dead

The first revision of this page listed three sites asking "is the non-moving
sweep the collector that will run?" and said they disagree. Two of them did.
The third could not have:

| site | predicate as written | what it actually evaluated to |
|---|---|---|
| `gen_heap.rs` `has_conservative_roots` | `is_active() \|\| unregistered_jit_frame_on_stack()` | as written — it runs after the root scan |
| `roots.rs` `conditional_loader_metadata` | `is_active() \|\| unregistered_jit_frame_on_stack() \|\| major_gc_requested()` | **`is_active() \|\| major_gc_requested()`** |
| `roots.rs` `conservative_locals_enabled` | `is_active()` | as written |

`collect_roots` calls `clear_unregistered_jit_frame_on_stack()` two statements
above `conditional_loader_metadata`, and only step 14 re-sets it. The disjunct
had been inert since it was added. `gc_quiescence::young_marker_follows_side_tables`'
own doc had recorded both halves of this — that the flag "is always `false` at
the mirror call site", and that `conditional_loader_metadata` "correctly never
had the term" — while the term sat in the tree. It is now removed, and that
comment is true again; `false` there is the safe direction (it keeps the
conservative unconditional mirror rooting).

So the real disagreement was two-way, and it is the one the fix closes.

## The fix

`collect_roots` gains **step 14a5**, immediately after step 14's
`scan_active_jit_frames`:

```rust
if a5_frame_pass_engages(conservative_locals) {
    conservative_frame_pass(shared, thread, &mut roots);
    a5_frame_pass::note(roots.len() - before);
}
```

`a5_frame_pass_engages` is `!step1_ran && compiled_in && unregistered_jit_frame_on_stack()`.

This is deliberately **not** the widening the page warned against. That warning
was right: reading the A5 flag where `conservative_locals` is computed answers
about the wrong cycle, and reordering the root scan to fix that is a large
change wanting a reproduction first. Running the same pass *after* step 14
needs neither — the flag is authoritative there, and the root vector is still
being built.

Soundness, which is stricter here than for step 1: the scan that sets the flag
also calls
`mark_moving_young_coverage_incomplete_because(UNREGISTERED_JIT_FRAME)`, and
`collect_garbage_inner` honours that through
`divert_for_incomplete_moving_coverage`, which overrides even
`CRATONVM_DBG_FORCE_MOVING`. On every cycle this branch fires, the young
collection provably does not relocate, so a false-positive root can only
over-retain — the exact condition `conservative_locals_enabled`'s doc states.

The pass is deliberately **not** folded into `publish_pinned_jit_roots`: those
are compiled-frame band words, these are interpreter frame slots.

Engagement is counted and printed under `CRATONVM_GC_STATS=1`:

```text
[GC] a5_frame_pass: cycles=<n> roots=<m>
```

`cycles=0` means every non-moving cycle in that run already had step 1's probe
on, so the repair decided nothing there — a reading a passing test cannot
otherwise be distinguished from "the repair works".

Unit tests (`vm/src/memory/roots.rs`):
`the_a5_pass_recovers_a_lost_tag_local_the_tag_filtered_scan_drops` (both
halves: the ordinary scan must NOT root a long-tagged slot, and the pass must)
and `the_a5_pass_engages_only_on_the_unregistered_jit_frame_path` (the truth
table, including that it stays off when the cycle may move).

### The peer deposit paths are deliberately unchanged

`interpreter::update_root_snapshot` and
`NativeContextImpl::deposit_root_snapshot_inner` also read
`conservative_locals_enabled()`. They run when a peer parks or blocks, long
before the cycle that consumes the snapshot picks a collector, so they cannot
make the "provably non-moving" argument above. Widening them would root a
pointer-shaped `long` into a snapshot a moving cycle may later remap, which
corrupts the number. They keep the `is_active()` gate.

## Instrument corrections made alongside

1. **The off-grid sweep-anchor counter was reporting the walk's own start.**
   The page noted two OTHER classes in the same suite run reporting
   `off_grid=1 anchors=2` while passing, and that the counter "is not" the
   exactly-zero its own message claims. It was a false positive, and the shape
   proves it: a two-entry anchor list is `[0, used]`, and `used` is unreachable
   from inside a `cursor < used` loop, so the only anchor it can have counted
   is offset 0 — which the anchor builder *deliberately exempts* from its own
   free-block filter ("it is the walk's start, not a split point"). The probe
   ran after `skip_free_blocks`, so a free block at the front of from-space
   made the walk resync past 0 and `continue`, and the next iteration counted
   it. Extracted as `gen_heap::AnchorGridProbe`, which consumes the exempt walk
   start up front and treats a resync over a KNOWN free block as accounted for
   rather than skipped — while still counting an anchor passed by an object
   stride, with four unit tests pinning exactly that split.
2. **The local-liveness ledger now carries its age.** `liveness_filtered_at` is
   keyed by address with no invalidation on re-serve, and the guard printed its
   hit as a broken contract ("the filter guarantees such a slot is never read
   again; it was"). Under GC stress that sentence gets printed about entries
   from a thousand collections earlier. Entries are now stamped with the
   collection they were made on and the report prints `collections_since`.
3. **An orphaned `#[cfg]`.** `dbg_fullstack_scan`'s doc comment and its
   `#[cfg(any(windows, linux))]` had been left behind when the function moved,
   and had attached themselves to `UNREG_MEMO_SUPPRESSED` — cfg-gating a
   counter `vm-cli` reads unconditionally. Reunited with the function.
4. **A stale blast-radius comment.** `conservative_locals_enabled` claimed its
   `real_forkjoinpool` gate was "OFF — the default for the entire app gauntlet",
   so the probe was "byte-identical to baseline". That flag defaults **on**
   (the synthetic pool is the opt-in); any argument built on that sentence was
   built on a default that no longer exists.

## What is and is not proven

**Measured on Linux x86-64 (Azure `vm1`), binaries built from `dev` @ `a58ebd5ca`.**

At this page's own configuration the class **passes**, and the collector never
takes the path the defect lives on:

| `CRATONVM_DBG_GC_STRESS` | young cycles | collector | result |
|---:|---:|---|---|
| 4 194 304 (this page's value) | 4 | `moving=4 non_moving=0` | PASS 27/27 |
| 2 097 152 | 8 | all moving | PASS 27/27 |
| 1 048 576 | 18 | all moving | PASS 27/27 |

`jit_active=false` and `unregistered_jit_frame=false` on **every** cycle of
every run: this workload is single-threaded, and the A5 probe's residue filter
declines the hit on this host. The Windows run that produced the report was in
the other regime — 6322 sweep cycles and 520–890 s is the *thrash* signature of
the non-moving sweep (`used` never retreats, so once it exceeds the stress
threshold every allocation triggers a collection), which is unreachable here
without forcing it.

Forcing it with the existing kill switch `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1`
reproduces the *regime* — the A5 accept, the non-moving sweep, and the thrash —
and the class still passes:

| binary | stress | cycles | collector | `a5_frame_pass` | result |
|---|---:|---:|---|---|---|
| pre-fix | 4 194 304 | 3551 | `moving=2 non_moving=3549` | — | PASS 27/27, 58.5 s |
| pre-fix | 1 048 576 | 8528 | `moving=13 non_moving=8515` | — | PASS 27/27, 70.7 s |
| pre-fix | 524 288 | 8568 | `moving=29 non_moving=8539` | — | PASS 27/27, 82.6 s |
| **fixed** | 4 194 304 | 3496 | `moving=2 non_moving=3494` | (fix1) | PASS 27/27, 70.2 s |
| **fixed** | 524 288 | 8568 | `moving=29 non_moving=8539` | **cycles=8384 roots=5248997** | PASS 27/27, 70.7 s |
| **fixed** | 4 194 304, unforced | 4 | `moving=4 non_moving=0` | cycles=0 roots=0 | PASS 27/27, 14.4 s |

The middle row is the one that matters: **8384 of that run's 8539 non-moving
cycles took the sweep on the A5 term with step 1's probe off**, and each ran
the repair. (The 155-cycle shortfall is cycles that were non-moving for a
different reason, or that already had `is_active()` — the pass declines both.)
The class passes with ~5.2 M extra conservative roots pushed across the run and
no wall-clock regression, which is the over-retention cost of the repair
measured rather than assumed. The last row is the negative control: with no A5
cycle the repair is inert.

Regression, default settings (no GC stress), 60 `core/spring-boot` classes
starting at index 100, `-Parallel 3`, Generational:

| binary | result | wall |
|---|---|---:|
| pre-fix | 59 PASS, 1 EMPTY (`AbstractPropertyMapperTests`, an abstract base with no tests) | 459.0 s |
| **fixed** | 59 PASS, 1 EMPTY (same class) | 419.3 s |

Unit tests: `cargo test --release -p cratonvm-vm --lib -- memory::roots` →
13 passed (including both new ones); `-p cratonvm-gc --lib -- anchor` →
8 passed (including the four new `AnchorGridProbe` ones).

So: **the gap is closed by construction, by unit test, and with the repair
measured engaging 8384 times on the exact collector state the report came from;
the specific Windows reclaim was not re-observed, because it does not occur on
the host available here.** Anyone with the Windows box should re-run the
original command and read `[GC] a5_frame_pass: cycles=` — a non-zero count
there is direct evidence that this repair engages on the cycle that failed.

## The page's own "Next" list, closed out

1. **"Establish whether the victim is a lost-tag operand at all."** Not
   directly, and it cannot be from this host — but the terminal only has one
   shape that produces it (see *Why a lost-tag slot is the shape that fits*),
   and the fix covers both candidates at once: the tag-independent probes AND
   the liveness-unfiltered local scan. If the victim was a liveness-filtered
   local rather than a lost-tag operand, step 14a5 roots it either way.
2. **`CRATONVM_DBG_FULLSTACK_SCAN=1`.** Moot as posed. That experiment asks
   whether the missed root was on the native stack outside the JIT chain's
   bands, and it can only answer on a run that reproduces. It does not
   reproduce here — and note that the A5 accept path already scans
   `[search_lo, stack_high)`, i.e. the whole band above the chain, so on the
   cycle in the report the native stack above the chain HAD been marked. That
   is itself evidence for "not on the stack", which is what step 14a5 assumes.
3. **"Do not run `--nojit` as a control."** Still correct, and now with a
   working substitute: `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` forces the A5
   accept and puts the collector in the non-moving regime **without** changing
   whether JIT frames exist, which is exactly the control `--nojit` cannot be.
   Three runs of it are in the table above.
4. **"Bisect the Mockito failure."** Below.

## Two things this page carried that went elsewhere

* **The Mockito failure** (`Mockito cannot mock this class: interface
  java.lang.annotation.Annotation`, `BindableTests.java:137`) does not
  reproduce here either: 27/27 in all nine runs above, including the three with
  thousands of non-moving sweeps. The page had already established it is not
  the reclaim (a third run failed with `guard=0`). It stays unexplained and
  Windows-only; re-check it with the same command.
* **A different defect that DOES reproduce here** is filed separately as
  `docs/known-issues/springboot/bindabletests-moving-young-leaves-a-frame-slot-unremapped-20260908.md`:
  the same class under harsher stress (`<= 262144`) fails deterministically
  after exactly 1203 **moving** cycles with an operand-stack slot the frame
  remap did not reach. It is the moving collector, not the sweep, so it is not
  this page — and folding it in here would have buried a repro that is
  deterministic on a host we have.

## Repro (unchanged)

```powershell
$env:CRATONVM_DBG_GC_STRESS  = '4194304'
$env:CRATONVM_DBG_SWEEP_ZERO = '1'      # names the victim class on the hit
$env:CRATONVM_GC_STATS       = '1'      # [GC] a5_frame_pass: cycles=…
run-spring-boot-suite.ps1 -ClassList <core/spring-boot BindableTests> -Parallel 1 `
  -TimeoutSec 1800 -Vm craton -CratonArgs @('--XX:UseGc','Generational')
```

`CRATONVM_GC_VERIFY_RSET` is NOT needed and costs a full old-generation walk per
collection. Do **not** use `--nojit` as a control: with no JIT frame the
collector takes the moving path, which frees nothing into a free list, so this
guard's free-list condition can never hold and the arm reads clean whatever the
truth is.
