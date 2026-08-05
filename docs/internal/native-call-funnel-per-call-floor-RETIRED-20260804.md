# The per-call floor is the native funnel — RETIRED 2026-08-04

| | |
|---|---|
| **Status** | RETIRED — every residual closed; the central attribution was wrong and is corrected below |
| **Was** | `docs/known-issues/vm/native-call-funnel-is-the-per-call-floor-20260803.md` (OPEN, high) |
| **Opened** | 2026-08-03, taking on [`aqs-thread-handoff-latency`](../known-issues/vm/aqs-thread-handoff-latency-20260803.md) |
| **Closed** | 2026-08-04, `perf/leaf-native-jit-bypass-20260804` |

The original document's shape was right and its measurements were right. Its
**attribution was wrong**, and it said so itself: *"Nobody has profiled it;
this document asserts where the time is, not which line."* Profiling it moved
the answer.

## What the original said, and what is true

| claim | verdict |
|---|---|
| An ordinary Java call is 8.4 ns; Java call dispatch is not the problem | **holds** |
| Natives cost 180-810 ns while Java calls cost 8.4 ns | **holds** |
| `Thread.onSpinWait`'s 4.4 ns proves the funnel, not the call, is the cost | **holds as a demonstration**, but see below — it proves *something around the call* is the cost, and the funnel turned out to be the smaller half of it |
| The funnel is ~180-330 ns fixed | **too high.** Measured at 110-128 ns for zero arguments on this host, and now ~40 ns |
| That cost is "~1000 cycles of bookkeeping, most of it diagnostics that are off by default" | **wrong.** The thirteen diagnostics are one `u32` test after ARCH-2026-08-04 A3, and the whole mask measures **1.7 ns**. The cost was two `Arc` refcount pairs |
| "808 ns for `Unsafe.compareAndSetInt` is funnel, not atomics" | **wrong in the same way.** The funnel was ~13 % of it; the rest is the JIT's native *dispatch* |

## Item 2 — the funnel, profiled

`vm/src/vm/vm_exec.rs`, module `native_funnel_profile` (`#[ignore]`d):

```bash
cargo test --release -p cratonvm-vm --lib funnel -- --ignored --nocapture
```

It drives `safe_native_call` with an `Ok(None)` callback against a bare
`SharedVm`, then times every component of the funnel body **on its own**. Last
of four passes, before the fix:

| rung | ns/op |
|---|---|
| `safe_native_call`, 0 args | 128.1 |
| **`2x thread_state::record_transition`** | **79.7** |
| the two `INLINE_NATIVE_ARGS` scratch arrays | 7.3 |
| `heap.load_and_forward` (object args only) | 16.4 |
| `catch_unwind` around the callback | 3.1 |
| `current_state()` alone | 3.6 |
| `native_diag_mask()` — all thirteen diagnostics | 1.7 |
| pin push + truncate | 3.3 |
| `value_as_validated_object_ref(Long)` | 1.1 |
| STW probe / young-pressure / JNI drain / `native_oom` | ≤ 2.1 each |
| the callback itself, with nothing around it | 0.8 |

The two thread-state transitions were **62 % of the funnel**. Everything the
document blamed — the diagnostics — was 1.3 %.

### Why the transitions cost that

`thread_state::record_transition` → `try_record_transition` → `with_cell`, and
`with_cell` **cloned** the thread's `Arc<ThreadStateCell>` out of TLS in order
to call a closure on the owned copy. One refcount increment and one decrement
per transition; `safe_native_call_impl` performs two transitions per native
call (`NativeRunning` on entry, the caller's prior state on return).

`current_state()` reads the same cell, through the same `RefCell`, in the same
TLS slot — and never clones. It measured 3 ns. That gap *was* the refcount
traffic.

`with_cell` now runs the closure against a borrow. Nothing about the census
changes: `CellHandle` still owns the `Arc`, the registry still holds its own,
and `CellHandle::drop` still removes the cell from `CELLS`.

### How it was priced — and why not with a before/after

A cross-run before/after cannot answer this. This is a shared build host: between
the two breakdown runs above, **every** rung moved, including `current_state()`
going 3.6 → 1.1 ns without being touched. A before/after here prices the box.

`threading::thread_state::with_cell_ab` (also `#[ignore]`d) prices it in one
process with the arms alternating on every pass, against the same TLS cell,
with `old_shape` a verbatim copy of the pre-fix body:

```text
with_cell   borrow (new): [3.47, 1.01, 1.65, 1.07, 1.83, 3.33]  min=1.01 ns
with_cell Arc::clone (old): [17.81, 17.47, 18.47, 18.54, 19.04, 19.36]  min=17.47 ns
```

The arms never cross — the new arm's *maximum* is below the old arm's
*minimum*. **16.5 ns per transition, twice per native call.**

This reaches far beyond the funnel: `record_transition` is also on every
safepoint and blocking transition in the VM.

## Item 1a — `Thread.currentThread()` from compiled code

The original recorded the interpreter fix as landed and the JIT half as the
larger, undone one — *"With the JIT on, the numbers do not move."* That is now
done: `jit_thread_current_thread_direct` (`vm/src/jit/helpers.rs`) answers the
call from `thread.java_thread_obj` with no funnel and no dispatch, bound as a
thin direct `CALL` by both backends.

Measured on `probes/NativeShapeProbe.java` with `scripts/ab-native-funnel.ps1`
(A-B-B-A interleaved, minimum of the last pass over three rounds):
**364.0 ns → 10.5 ns, 34.7x** — and 315.2 → 7.9 ns, 39.9x on a separate
two-round run. The run-time bypass counter reads 7,993,000 for a loop that
makes 8,000,000 calls.

### There are THREE compile doors, and a bypass has to be bound in all of them

This is the reusable lesson of the item, and it cost two full build-and-measure
cycles to learn:

1. `jit::try_compile_inner`'s **single-pass** ladder — where all seven
   pre-existing `*_DIRECT_FN` thin helpers are recognised.
2. `jit::try_compile_inner`'s **IR / optimizing** eligibility loop — a
   *separate* loop that asks `callee_compiler`, which answers `None` for a
   registered native because a native has no compiled body to bind. No route to
   the thin helpers at all.
3. `vm::runtime::interpreter::jit_bridge::compile_osr_artifact` — the **OSR
   door**, which "reaches `x64::compile_with_param_slots` directly rather than
   through `jit::try_compile`" (its own comment) and carries its **own copy** of
   the ladder.

A hot loop is compiled by door 3. The first cut of this fix bound door 1; the
second bound doors 1 and 2. Both were **completely inert** — 0 bypasses on
8,000,000 calls.

The instrumentation that settled it is worth keeping in mind, because the
obvious counter is not enough. A run-time "did the fast path fire" counter
reading 0 has two causes that nothing else separates: the ladder ran and the
triple did not match, or the ladder never ran at all. Adding the *denominator*
— how many `invokestatic` sites each ladder examined — answered it in one run:
**0/0 and 0/0**. Neither `try_compile_inner` ladder had looked at a single
static call site in the whole process, which pointed straight at a door nobody
had listed.

Both counters are permanent, reported together under
`CRATONVM_INTRINSIC_STATS=1`.

> **A live finding, not history.** The other six thin helpers are bound in
> doors 1 and 3 but **not** door 2 — so in optimizing-tier method compiles
> (as opposed to OSR compiles) `Integer.valueOf`'s direct call, the very
> precedent the original document quotes for all of this, does not fire. That
> is a separate lever and is deliberately *not* pulled here: each one changes
> what the optimizing tier emits on a measured hot path, and none has been
> A/B'd at that tier.

### The counters, and why there are three

`CRATONVM_INTRINSIC_STATS=1` reports:

```text
[cratonvm] interpreter intrinsic dispatches: N
[cratonvm] compiled-code native-funnel bypasses: N (Thread.currentThread sites bound: single-pass S, IR I)
[cratonvm] leaf-native funnel-free dispatches: N (audit violations: V)
```

The site counts are **compile-time**; the bypass count is **run-time**. They
answer different questions and both are needed: sites-bound `== 0` means the
recognition never ran, while sites-bound `> 0` with bypasses `== 0` means it ran
and the emitted call is not being taken. Nothing else separates those two, and
the first cut failed in the first way.

## Item 1b — LEAF natives, as a registration property

`native-api/src/leaf.rs`. A native is a leaf when, for **every** input, it
allocates nothing on the Java heap and starts no collection; reaches no
safepoint and never blocks; raises no Java exception and calls no Java code;
and retains no `ObjectRef` past its return.

The first two are the load-bearing ones. A native that cannot allocate and
cannot safepoint cannot be running while the collector moves anything — a peer
STW waits for this thread to reach its next poll, and this native reaches none
— so its arguments cannot go stale under it and there is nothing for
`native_pin_roots` to protect.

The original asked for the predicate to *"live on the registration (a
`NativeKind`-adjacent flag), not in a growing `match` in `helpers.rs`"*, and it
does: one auditable table, consulted once per registration. Because every
dispatch route — the interpreter's inline cache, the JIT's dispatch helper, and
`invoke_or_native`'s twenty-nine call sites — passes through
`safe_native_call`, and the flag is read *there*, marking a triple changes all
of them at once.

Reading it back costs the ~3,100 **non**-leaf natives one relaxed load, a shift
and a test: `leaf::is_leaf_callback` is a one-word bloom filter over callback
addresses, and a clear bit is a proof of non-leaf. Only a set bit consults the
exact table.

### The table is deliberately two entries long

`System.nanoTime()` and `System.currentTimeMillis()`. Both were read
end-to-end; both touch no `ctx` at all.

Candidates that were considered and **rejected**, because the predicate is
about every branch and not the hot one:

* `System.identityHashCode` — no allocation, but it takes an object argument
  whose heap-membership validation is exactly what the funnel's pinning does.
  Not worth reasoning about a wild ref for ~25 ns.
* `Unsafe.compareAndSetInt` — the AQS case is a plain
  `compare_and_swap_field`, but the null-object arm takes a shard lock and the
  synthetic-offset arm goes through an identity-hash side table.
* `AbstractOwnableSynchronizer.setExclusiveOwnerThread` — takes the thread
  registry lock via `record_jmx_owned_synchronizer`.

The mechanism is the deliverable; the table is a list that grows one verified
entry at a time.

### Identical code folding is the sharp edge of keying on the address

Two Rust `fn` items with identical machine code may share one address — MSVC's
`/OPT:ICF` is on by default in release builds — so distinct `fn` items are not
a guarantee of distinct addresses, and marking one leaf callback also marks
whatever the linker folded with it.

This is not theoretical. It is how `leaf_native_tests` first failed (three
same-bodied test natives collapsed into one address), and the *same phenomenon*
independently broke two unrelated JNI tests in this tree —
`jni_function_table_extended_to_234` and `jni_nio_slots_not_stub` assert
`slot[233] != jni_stub` and fail because `jni_get_module` folds with the stub
(diagnosed 2026-08-01 on `fix/locale-tostring-shadow-20260801`, still unmerged;
both were already red on `dev` before this work and remain so).

It is not a live hazard for the current table — both entries read a clock and
nothing else compiles to the same bytes — and it cannot be one for a correctly
chosen entry, since a native folded with a leaf has byte-identical code and so
does byte-identically nothing dangerous. It becomes one the moment somebody
adds an entry whose body is a trivial constant return, which is the shape
hundreds of registered stubs share. The note is in `leaf.rs` where an author
adding an entry will read it.

### The audit is shown to fail

A mis-marked leaf is a silent heap-corruption bug, so the claim is checkable
rather than asserted. `CRATONVM_DBG=leafaudit` runs a marked native through the
**full** funnel — so an audited run stays correct — and then checks all four
claims: no collection ran, the pin stack is unchanged, nothing was published to
`native_pending_return`, and the native neither threw nor returned an object.

`the_audit_catches_a_native_that_is_not_really_a_leaf` (vm_exec tests) hands
the audit a native that allocates and returns the object, and asserts the
violation count rises. Without that injection, a clean `0` proves nothing —
a guard that cannot fail reads exactly like one that works.

## Item 3 — `Unsafe.compareAndSetInt`

The original: *"808 ns for it is funnel, not atomics."* Both halves are wrong.
It is not atomics, and it is mostly not funnel either — with the funnel
measured at ~110 ns pre-fix and ~40 ns post-fix, a rung in the 800-1500 ns
range is dominated by neither.

**Where it actually goes** is the JIT's native dispatch: an `invokevirtual`
whose callee is a registered native binds no compiled body, so it falls
through `jit_invoke_dispatch` to `vm_exec::invoke_or_native`, which re-resolves
the callee **by name** on every call — a dozen string comparisons for special
cases, then a three-hash registry probe — and only then enters the funnel.
`jit_invoke_dispatch`'s own per-callsite native cache
(`OBJECT_NATIVE_DISPATCH_CACHE`) exists and would skip all of that, but serves
only three hand-written families (`HashMap`, `Matcher`, `StringBuilder`) —
which is itself the "growing `match` in `helpers.rs`" the original objected to,
in the one place where generalising it would pay.

That generalisation is the real remaining lever for native-dense compiled code.
It is **not** attempted here, and the reason is scope rather than difficulty:
it changes native-versus-bytecode precedence for every compiled call site in
the VM, and the suites that would catch a regression in it (Spring Boot,
Tomcat) run on the Linux host, not here. Recorded so the next person starts
from the measurement instead of from the original document's premise.

## The end-to-end A/B, and what it can and cannot resolve

`scripts/ab-native-funnel.ps1`, A-B-B-A interleaved over three rounds,
minimum of the last pass per rung, base = `origin/dev`:

| rung | base | fix | ratio |
|---|---|---|---|
| NATIVE static, no arg -> OBJ: `currentThread` | 364.0 | **10.5** | **34.7x** |
| NATIVE static, no arg -> long: `nanoTime` | 342.1 | 164.0 | 2.09x |
| NATIVE static, OBJ arg -> int: `identityHashCode` | 533.7 | 273.6 | 1.95x |
| INTRINSIC recv: `String.length()` | 844.4 | 547.3 | 1.54x |
| INTRINSIC static: `Math.abs(int)` | 230.1 | 204.8 | 1.12x |
| NATIVE recv, no arg: `AtomicInteger.get` | 690.0 | 696.0 | 0.99x |
| NATIVE recv, prim args: `Atomic.CAS` | 872.7 | 944.9 | 0.92x |
| interpreter-answered `Thread.onSpinWait` (control) | 4.8 | 4.9 | 0.98x |
| no call (control) | 0.8 | 0.7 | 1.14x |

**Only the `currentThread` row is a measurement.** Everything else on this
table is noise, and saying so is the honest reading rather than a hedge:

* The host was running other builds throughout. Between this run and a
  two-round run twenty minutes earlier, `nanoTime`'s base moved 257 → 342 ns
  and its ratio 1.44x → 2.09x. A control rung moved 1.00x → 1.14x.
* `String.length()` at 1.54x is the tell. It is an interpreter intrinsic that
  this branch does not touch at all, so a 1.5x "improvement" on it is the
  measurement's own noise floor — which means every other sub-2x row is inside
  it too.
* The two `AtomicInteger` rungs reading *slower* are the same noise with the
  opposite sign. They are also where the change is expected to be least
  visible: at ~900 ns per call the whole funnel is now ~40 ns, so even
  deleting it entirely would move them ~4 %.

This is exactly why the two claims this document does make are backed by
**in-process** measurements instead — `with_cell_ab` and
`funnel_cost_breakdown`, both of which run their arms in one process against
one VM — and why the third is backed by a counter rather than a clock.

## Test-suite state, and the four instances of one trap

`cargo test --release` on this branch:

| crate | result |
|---|---|
| `cratonvm-vm --lib` | 2387 passed, **2 failed** |
| `cratonvm-jit` | 1942 passed, **1 failed** |
| `cratonvm-native-api`, `cratonvm-types` | pass, except `doc_citation_paths` |

Every failure is pre-existing and each was **checked**, not assumed:

* `jni_function_table_extended_to_234` and `jni_nio_slots_not_stub` — already
  diagnosed on 2026-08-01 in commit `dca43a8c3` ("slot 233 folds with the stub,
  so stop asserting it does not"), which sits on the unmerged branch
  `fix/locale-tostring-shadow-20260801`.
* `ir_lower::tests::a_wide_field_read_refuses_without_the_sentinel_disambiguator`
  — verified pre-existing by reverting `jit/src/lib.rs` to `origin/dev` and
  re-running: it fails identically. Its two `extern "C"` fakes both compile to
  `xor eax,eax; ret`.
* `doc_citation_paths` — red on `dev`. This branch **reduces** one of its two
  failures from 37 flagged lines to 28; the remaining 28, and all four dead
  `array-class-defining-loader.md` citations, are untouched pre-existing ones.

Three of those four are the same trap, and it is worth naming because it is
invisible in source: **`/OPT:ICF` folds functions with identical machine code,
so distinct `fn` items are not distinct addresses.** It broke two JNI tests, one
IR test, and the first cut of this branch's own leaf tests. Any assertion of the
form `assert_ne!(fn_a as usize, fn_b as usize)` is unassertable unless the two
bodies differ.

## What landed

* `vm/src/threading/thread_state.rs` — `with_cell` borrows instead of cloning.
* `vm/src/vm/vm_exec.rs` — the leaf route, `safe_native_call_leaf`, the audit,
  and the `native_funnel_profile` breakdown.
* `native-api/src/leaf.rs` — the LEAF class, its table and its bloom filter.
* `native-api/src/registry.rs` — marks leaf callbacks at registration.
* `vm/src/jit/helpers.rs` — `jit_thread_current_thread_direct` + counter.
* `jit/src/lib.rs` — recognition in **both** backends, and the compile-time
  site counters.
* `vm-cli/src/main.rs` — all three counters under `CRATONVM_INTRINSIC_STATS=1`.
* `scripts/ab-native-funnel.ps1` — the interleaved A/B runner.

## Reproduction

```powershell
$jdk = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& "$jdk\bin\javac.exe" -d out probes\NativeShapeProbe.java probes\CallShapeProbe.java `
                              probes\CallFloorConvergenceProbe.java probes\LockNativeCensusProbe.java
& "$jdk\bin\java.exe" -cp out NativeShapeProbe                      # HotSpot control
powershell -File scripts\ab-native-funnel.ps1 -Base <before.exe> -Fix <after.exe>
```

`CallShapeProbe`'s HotSpot column reads 0.00 throughout — HotSpot eliminates
the loops outright. That arm is a sanity check only; use `NativeShapeProbe` for
cross-VM comparison.
