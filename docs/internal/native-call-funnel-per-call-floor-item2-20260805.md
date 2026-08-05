# The native funnel's item 2, actually profiled — and it was not the diagnostics

| | |
|---|---|
| **Status** | CLOSED — item 2 done, item 3's remaining claim corrected |
| **Amends** | [`native-call-funnel-is-the-per-call-floor-RETIRED-20260805.md`](native-call-funnel-is-the-per-call-floor-RETIRED-20260805.md) |
| **Landed** | 2026-08-05, `perf/leaf-native-jit-bypass-20260804` |

The retired record closed items 1 and 3 of the original brief with the LEAF
class, and left item 2 open in as many words:

> Nobody profiled it here either… Trimming that further is a real but separate
> piece of work, and it should start from a profile, not from this document's
> assertion.

This is that profile, and the fix it named. It is a companion, not a
replacement: nothing here contradicts the leaf work, and the two are
complementary by construction — the leaf path removes the funnel for the
natives that can skip it, and this removes most of what is left for the ~3,100
that cannot.

## The profile

`vm/src/vm/vm_exec.rs`, module `native_funnel_profile`, `#[ignore]`d:

```bash
cargo test --release -p cratonvm-vm --lib funnel -- --ignored --nocapture
```

It drives `safe_native_call` with an `Ok(None)` callback against a bare
`SharedVm`, then times each component of the funnel body **on its own**. Last
of four passes, before the fix:

| rung | ns/op |
|---|---|
| `safe_native_call`, 0 args | 128.1 |
| **`2x thread_state::record_transition`** | **79.7** |
| `heap.load_and_forward` (object args only) | 16.4 |
| the two `INLINE_NATIVE_ARGS` scratch arrays | 7.3 |
| `current_state()` alone | 3.6 |
| pin push + truncate | 3.3 |
| `catch_unwind` around the callback | 3.1 |
| `disable_jit()` + `native_array_gc` swap | 2.1 |
| **`native_diag_mask()` — all thirteen diagnostics** | **1.7** |
| `value_as_validated_object_ref(Long)` | 1.1 |
| STW probe / young-pressure / JNI drain / `native_oom` | ≤ 1.5 each |
| the callback itself, with nothing around it | 0.8 |

The original brief blamed "~1000 cycles of bookkeeping, most of it diagnostics
that are off by default". The diagnostics are **1.3 %** of the funnel — ARCH-A3
had already dealt with them, and the retired record says so. What the brief
could not have known is that the answer was somewhere else entirely: the two
thread-state transitions were **62 %**.

## What the transitions were actually doing

`record_transition` → `try_record_transition` → `with_cell`, and `with_cell`
**cloned** the thread's `Arc<ThreadStateCell>` out of TLS in order to call a
closure on the owned copy. One refcount increment and one decrement per
transition; the funnel performs two per native call (`NativeRunning` on entry,
the caller's prior state on return).

`current_state()` reads the same cell, through the same `RefCell`, in the same
TLS slot — and never clones. It measured 3 ns. That gap *was* the refcount
traffic.

`with_cell` now runs the closure against a borrow. Nothing about the census
changes: `CellHandle` still owns the `Arc`, the registry still holds its own,
and `CellHandle::drop` still removes the cell from `CELLS`. The cold arm scopes
the shared borrow before taking the mutable one — an `if let` scrutinee
temporary otherwise lives to the end of the whole `if/else`, and `borrow_mut()`
would panic on a thread's first transition.

## How it was priced, and why not with a before/after

A cross-run before/after cannot answer this. The build host is shared: between
the two breakdown runs above, **every** rung moved — `current_state()` went
3.6 → 1.1 ns without being touched. A before/after here prices the box.

`threading::thread_state::with_cell_ab` (also `#[ignore]`d) prices it in one
process, with the arms alternating on every pass, against the same TLS cell,
with `old_shape` a verbatim copy of the pre-fix body:

```text
with_cell   borrow (new): [3.47, 1.01, 1.65, 1.07, 1.83, 3.33]  min=1.01 ns
with_cell Arc::clone (old): [17.81, 17.47, 18.47, 18.54, 19.04, 19.36]  min=17.47 ns
```

The arms never cross — the new arm's *maximum* is below the old arm's
*minimum*. **16.5 ns per transition, twice per native call.**

Re-run after merging `origin/dev` (leaf work included) on a quiet host, where
everything is faster and the ratio is larger: old `min=11.19 ns`, new
`min=0.60 ns`, **10.59 ns of separation** per transition. Both runs are
drift-free within themselves, which is the point of the shape; the absolute
numbers move with the box and the separation does not vanish.

The post-merge funnel breakdown on that same quiet host, for scale: a
one-argument `safe_native_call` is now **22.9-29.7 ns** total, of which the two
transitions are 8-12 ns. Before the fix those two alone would have added ~22 ns
to it.

This reaches well beyond the funnel. `record_transition` is on every safepoint
and every blocking transition in the VM.

## Item 3 — the last claim, corrected

The retired record leaves item 3 open with:

> Its 808 ns is still mostly funnel.

That is measurably false. The funnel is ~110 ns before this change and ~40 ns
after, so a `Unsafe.compareAndSetInt` rung in the 800-1500 ns range is
dominated by neither the funnel nor the atomics.

Where it goes is the JIT's native **dispatch**: an `invokevirtual` whose callee
is a registered native binds no compiled body, so it falls through to
`vm_exec::invoke_or_native`, which re-resolves the callee **by name** on every
call — a dozen string comparisons for special cases, then a three-hash registry
probe — and only then enters the funnel. `jit_invoke_dispatch`'s per-callsite
native cache (`OBJECT_NATIVE_DISPATCH_CACHE`) would skip all of it but serves
only three hand-written families (`HashMap`, `Matcher`, `StringBuilder`).

Generalising that cache is the real remaining lever for native-dense compiled
code, and it is deliberately not attempted here: it changes
native-versus-bytecode precedence for every compiled call site in the VM, and
the suites that would catch a regression in it run on the Linux host.
Item 3 stays open — but for a different reason than the one recorded, and the
next attempt should start from the dispatch path, not the funnel.

## `Thread.currentThread()` — one level below the leaf path

The leaf work serves `Thread.currentThread()` from inside the JIT dispatch
helpers, which measured 47-61 ns. The compilers can do better: it is a
statically bound `invokestatic` whose answer is one field read of an existing
per-thread GC root, so it can be a baked direct `CALL` that reaches no dispatch
helper at all. `jit_thread_current_thread_direct` is that call, and it measured
**364.0 → 10.5 ns (34.7x)** against `origin/dev` before the leaf work landed.

It matters because the JDK calls it constantly — twice per uncontended
`ReentrantLock.lock()`/`unlock()` pair, censused with `--dump-native-registry`.

On the merged tree (leaf work + this), quiet host, `NativeShapeProbe` last of
four passes, with both counters confirming both mechanisms are live —
23,983,036 compiled leaf dispatches **and** 7,992,000 direct
`Thread.currentThread` calls:

| rung | original brief | merged |
|---|---|---|
| `Thread.currentThread` | 409 | **7.8** |
| `String.length` | 411 | 13.5 |
| `Math.abs` | 184 | 58.2 |
| `System.nanoTime` | 326 | 72.1 |
| `AtomicInteger.get` | 489 | 104.0 |
| `System.identityHashCode` | 361 | 240.2 |
| `AtomicInteger.CAS` | 808 | 437.6 |

The last two rows are the ones with no leaf claim and no direct bind — they are
the population still on the funnel, and they are where item 3's remaining work
lives.

### There are THREE compile doors

This is the reusable half, and it cost two full build-and-measure cycles:

1. `jit::try_compile_inner`'s **single-pass** ladder — where all seven
   pre-existing `*_DIRECT_FN` thin helpers are recognised.
2. `try_compile_inner`'s **IR / optimizing** eligibility loop — a *separate*
   loop that asks `callee_compiler`, which answers `None` for a registered
   native because a native has no compiled body to bind. No route to the thin
   helpers at all.
3. `jit_bridge::compile_osr_artifact` — the **OSR door**, which "reaches
   `x64::compile_with_param_slots` directly rather than through
   `jit::try_compile`" (its own comment) and carries its **own copy** of the
   ladder.

A hot loop is compiled by door 3. Binding door 1 was inert. Binding doors 1+2
was inert. Both reported **0 bypasses on 8,000,000 calls**, and no timing could
have told that apart from "installed but no faster".

What separated them was adding the **denominator** — how many `invokestatic`
sites each ladder examined. It read `0/0` and `0/0`: neither `try_compile_inner`
ladder had looked at a single static call site in the whole process, which
pointed straight at a door nobody had listed. Both counters are permanent and
reported per door by `CRATONVM_INTRINSIC_STATS=1`.

> **Still live:** the other six thin helpers are bound in doors 1 and 3 but not
> door 2, so in optimizing-tier *method* compiles `Integer.valueOf`'s direct
> call — the precedent the original brief quotes for all of this — does not
> fire. Not pulled here: each one changes what the optimizing tier emits on a
> measured hot path, and none has been A/B'd at that tier.

A related trap found in the same change: a thin helper's address must **not**
go into `_direct_callee_entries`. That list is the keep-alive set
`prepare_for_publication` pins, and it refuses to publish a body whose baked
target it cannot resolve to a live JIT artifact — which a process-lifetime
`extern "C"` function never will.

## `/OPT:ICF` — four tests, one cause

Identical COMDAT folding merges functions with identical machine code, so
**distinct `fn` items are not distinct addresses**. Any assertion of the form
`assert_ne!(fn_a as usize, fn_b as usize)` is unassertable unless the bodies
differ. In this tree it accounts for:

* `jni_function_table_extended_to_234` and `jni_nio_slots_not_stub` —
  `jni_get_module` folds with `jni_stub`. Diagnosed 2026-08-01 in `dca43a8c3`,
  on the still-unmerged `fix/locale-tostring-shadow-20260801`.
* `ir_lower::tests::a_wide_field_read_refuses_without_the_sentinel_disambiguator`
  — its two `extern "C"` fakes both compile to `xor eax,eax; ret`. Verified
  pre-existing by reverting `jit/src/lib.rs` to `origin/dev` and re-running.
* the first cut of this branch's own tests, where three same-bodied test
  natives collapsed into one address.

## The end-to-end A/B, and what it cannot resolve

`scripts/ab-native-funnel.ps1` runs `NativeShapeProbe` A-B-B-A and reports the
minimum last-pass ns/op per rung. On the three-round run against `origin/dev`
(pre-leaf-work), only the `currentThread` row (34.7x) is a measurement.
`String.length()` — an interpreter intrinsic this branch does not touch — moved
1.54x in the same run. That is the noise floor, so every sub-2x row sits inside
it, in both directions: two `AtomicInteger` rungs read 0.92-0.99x, which is the
same noise with the opposite sign.

Which is why the claims this record makes are backed by **in-process**
measurements (`with_cell_ab`, `funnel_cost_breakdown` — arms in one process,
one VM) and by counters, not by that table.
