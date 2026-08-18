# A compiled call reaches its compiled callee through a Rust helper — two causes, both closed

**Status: RETIRED 2026-08-17.** Filed the same day out of
`vm-per-call-dispatch-cost-RETIRED-20260817.md`, and retired here after both
causes were settled — one of them by a commit that landed 38 minutes after the
page was written, which is why the page's own census could not see it.

Both causes are stated as counts, and both counts moved. The evidence for each
is an A/B **on one binary**, because the page's own measurement note is right:
the class clock on the shared Azure host measured 52.3 s, 56.9 s, 67.8 s and
72.5 s for the same binary on the same class in one afternoon, and every number
below would have drowned in that.

---

## Cause 1 — a call site is offered a direct `CALL` exactly once

### Closed by `734dd4035`, which the page predates by 38 minutes

The page's shape was exactly right: a statically bound site is offered a direct
machine `CALL` while its **caller** is compiled, and if the callee is not
compiled at that instant the site stays on `jit_invoke_dispatch` forever — "not
a property of the call; a property of the order in which two methods happened to
cross their compile thresholds."

`734dd4035` ("eager callee compilation is transitive, so a bind stops depending
on compile order") made `direct_callee_lookup` COMPILE an uncompiled callee
rather than decline it, bounded by depth, cycle and fan-out. It is default-ON
(`CRATONVM_JIT_EAGER_CALLEE_CHAIN=0` opts out), so the lever is a one-binary
A/B of exactly the tree the page measured.

`io.netty.buffer.BigEndianHeapByteBufTest`, one binary, `CRATONVM_DBG=mic-prof,intrinsic-stats`:

| counter | the page | chain OFF | chain ON (dev default) |
| --- | ---: | ---: | ---: |
| direct callee binds, bound | 666 | 674 | **2048** |
| direct callee binds, left on the helper | **1394** | **1396** | **892** |
| `kind_static` | 107 874 082 | 107 885 938 | **1 707 910** |
| `kind_special` | 5 475 486 | 5 481 392 | **284 910** |
| `out_dcache` | 111 543 628 | 111 565 227 | **367 456** |
| `out_site_native` | 1 507 946 | 1 507 131 | 1 519 536 |
| `out_tail` | 259 051 | 253 766 | 98 940 |
| class wall | — | 42 230 ms | 32 943 ms |
| result | — | 414/414 | 414/414 |

The chain-OFF column reproduces the page to within 0.02% on every counter it
printed, which is what says the middle column IS the page's tree and the right
column is what changed. **`out_dcache` — the whole subject of the page — falls
by 303x**, and the page's own acceptance criterion ("a fix that does not move
`1394` toward `0` has not landed") is met.

### WHICH gate refused, which the page could not ask

A bare miss total cannot separate a compile-ORDER accident (repairable by
re-binding) from a standing policy refusal (not), and those want opposite fixes.
`DirectBindRefusal` (`jit/src/lib.rs`) now tallies the reason at both
`callee_compiler` doors and `CRATONVM_DBG=intrinsic-stats` prints the breakdown
beside the total. Same class, same binary:

| refusal | chain OFF | chain ON | chain ON + `DIRECT_EXC_TABLE_PUBLISH=1` |
| --- | ---: | ---: | ---: |
| `callee-not-yet-compiled` | **949** | **0** | 0 |
| `native-shadow` | 451 | 736 | 760 |
| `callee-class-not-found` | 54 | 85 | 97 |
| `indy-trap` | 10 | 31 | 36 |
| `callee-exception-table` | 15 | 16 | **0** |
| `eager-callee-chain-{depth,cycle,budget,compile-declined}` | — | 20 | 24 |
| `synchronized` | 3 | 3 | 3 |
| `declaring-class-not-initialized` | 2 | 3 | 3 |
| `unattributed` | 0 | 0 | 0 |

`callee-not-yet-compiled` is Cause 1 stated as a counter, and it is gone. Read
the counts as *sites examined across a whole run*, not as a fixed population:
binding more callees makes more callers compile, which is why the standing-gate
rows grow while the repairable row empties.

### What is left, and why none of it is Cause 1

`native-shadow` (736, 82% of the residual) cannot bind to a Java entry by
construction — a Rust native shadows that method, and those calls are served by
the leaf-native site cache (`out_site_native` = 1.5 M), not by a wasted round
trip. `callee-class-not-found`, `indy-trap`, `synchronized` and
`declaring-class-not-initialized` are standing correctness gates, each with its
own reason at its own site. The four `eager-callee-chain-*` rows are the chain's
own bounds, deliberately conservative. Only `callee-exception-table` was a
policy that had outlived its reason, and that is Cause 2 — see below.

---

## Cause 2 — a callee that declares an exception table is barred from the inline cache

### The bar was a correctness bar. It had stopped being one.

The page states the mechanism a fix needs: "The cascade must recognise the
sentinel after the `CALL` and route it through the callee's own table — a
compare-and-branch to a stub plus the Rust routing the helper path already
performs."

That is `Compiler::emit_inline_callee_deopt_check` (`jit/src/x64/deopt_stubs.rs`)
plus `jit_service_callee_deopt` (`vm/src/jit/helpers.rs`), and both were already
there. The check is emitted after **every** inline direct-entry `CALL` — each PIC
slot, the MIC arm, the megamorphic hashed stub's `emit_callee_deopt_check` twin,
and the baked `invokestatic`/`invokespecial` direct calls — and hands the
`i64::MIN` sentinel to `handle_compiled_callee_deopt_sentinel`, the same routine
every Rust helper arm uses. It landed later, for the H2 `MVMap`/`DataType.read`
case, and the ban was never revisited against it. The page inherited the ban's
original justification, which by then described code that no longer existed.

### Proving that, rather than trusting the comment that says it

`probes/CalleeExceptionTableSemanticsProbe.java` drives six exception-table
callees through a monomorphic interface site — implicit AIOOBE, NPE and divide,
an explicit `athrow`, a table that does NOT cover what it throws, and a
`finally` — checking the exact accumulator and the exact body-execution count for
every round, so a swallowed, duplicated or mis-routed exception cannot average
away.

| arm | result |
| --- | --- |
| HotSpot 25 (the oracle) | PASS |
| ban kept | PASS |
| ban lifted | PASS |
| ban lifted **+ `CRATONVM_JIT_SP_IC_DEOPT_CHECK=0`** | **FAIL — `ArithmeticException` escapes `Div.apply`'s own `catch` to `main`** |
| ban kept + `SP_IC_DEOPT_CHECK=0` | PASS |

The fourth row is the point. Deleting the sentinel check is the only way found to
make the lifted ban wrong, which is what says the check is what makes it right.
The fifth row is its control: with the ban kept, deleting the check changes
nothing, because nothing is published.

**The first version of this probe was blind and passed all five.** It warmed the
site with a loop and then made the throwing call after it, from a frame that was
never compiled — so no throwing call ever reached the cascade. Every throwing
call now happens inside the hot loop, at the same site the warm-up published.

### What lifting it buys

`probes/NativeFunnelFloorProbe.java`, ABBA on one binary, ns/op. The two rungs
differ by one never-taken `try`/`catch` in the callee and nothing else:

| rung | ban kept | ban lifted |
| --- | ---: | ---: |
| interface call, callee has no exception table (control) | 15.75 / 15.91 | 15.65 / 15.60 |
| interface call, callee has `try`/`catch` | **125.98 / 125.94** | **14.35 / 14.56** |

**8.7x**, with the control rung unmoved. Repeated on the fixed binary at a later,
busier hour — 189.42 / 200.46 against 24.44 / 24.42, control 25.93 / 27.00 — the
absolute numbers move with load, the ratio does not.

The page recorded 11.4x for the same pair; the difference is that the Rust-level
entry cache (`try_mic_rust_cached_entry`) had since taken part of the barred
arm's cost off, which is why the barred arm reads 126 ns here and 221 ns there.
The remaining gap is the machine-code cascade itself.

### The default is now ON, with an interlock

`mic_publish_exception_table_callees()` is default-ON since 2026-08-17;
`CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=0` restores the ban. It refuses to publish
outright unless `sp_ic_deopt_check_mode() == On`, so the unsound combination the
fourth probe arm demonstrates is not reachable by setting one switch — both
`SP_IC_DEOPT_CHECK=0` and `=void` make the gate answer no, and the probe passes
in both.

`=void` matters specifically: it suppresses the check for exactly the void
callees whose return register carries no value, which is exactly where a void
exception-table callee would have gone unserviced.

### Honest scope: this is a per-call win, not a per-workload one, on THIS class

On `BigEndianHeapByteBufTest` the change moves 18 336 calls out of 6.37 M
(`mic_rust_cache` 18 336 → 0, the counter for calls the bar pushed onto the Rust
entry cache). At ~110 ns each that is ~2 ms of a 33 s run. The 8.7x above is a
per-call ratio on a probe built to isolate the shape; it is not a claim about
this class, and the page's own warning applies — size it with the counter.

Where it is worth more is where that counter is large. `mic_rust_cache` in a
pre-fix binary is the targeting instrument for finding those workloads, and it is
printed by `CRATONVM_DBG=mic-prof` on any run.

**A counter that read zero and meant nothing.** `MIC_PROF`'s `pub_barred` reads
`0` on both arms of this class, which looks like "the bar never fired". It is
bumped at only one of the two publish sites in `jit_invoke_virtual_mic`; the
other one published 18 336 barred entries into the Rust cache without touching
it. Use `mic_rust_cache` from `[DISP_CENSUS]`, not `pub_barred`.

---

## The same bar in the other door

Both `callee_compiler` ladders refused an exception-table callee for the same
stated reason, and the same answer applies: the baked direct `CALL` carries
`emit_inline_callee_deopt_check` too. `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH=1`
lifts it, default-OFF, interlocked on `SP_IC_DEOPT_CHECK` the same way.

A direct `CALL` has one precondition the inline cascade does not: the emitter
must have reserved the contiguous service-argument slots the check reads. A site
that could not was previously emitted anyway, with the hazard only printed under
`CRATONVM_DBG_DEOPT` — the unserviced raw edge `dbg_unserviced_direct_call`'s own
comment calls the birthplace of an orphaned deopt frame. It is now a compile
failure (`direct-call-service-slots`), so "the ladder bound this callee" implies
"a trap here is serviced", with no remaining case. Measured cost of the stricter
rule on this class: all 49 unserviced direct calls carry `info=false` — inline
intrinsics and thin native helpers, which have no `JitInvokeInfo` and no way to
stash a frame — so none is a Java callee and nothing there fails.

Effect on the class, one binary:

| counter | lever off | lever on |
| --- | ---: | ---: |
| bind refused, `callee-exception-table` | 16 | **0** |
| binds bound | 2079 | 2098 |
| `kind_special` | 285 521 | 254 941 |
| `out_dcache` | 369 233 | 342 430 |
| result | 414/414 | 414/414 |

---

## Instruments this left behind

* `CRATONVM_DBG=intrinsic-stats` now prints `bind refused, <reason>: <n>` per
  refusal reason plus an `unattributed` remainder — the question "which gate"
  is answerable in one run.
* `probes/CalleeExceptionTableSemanticsProbe.java` — six exception-table shapes
  through a published inline cache, with `SP_IC_DEOPT_CHECK=0` as its own red
  proof. Run it against HotSpot first; it is written to be an oracle comparison,
  not a self-check.
* `CRATONVM_JIT_EAGER_CALLEE_CHAIN=0` reproduces the pre-`734dd4035` tree on any
  current binary, which is how this page's whole census was reproduced without a
  second build.

## What is NOT closed

Nothing from this page. The residual 892 refused binds are named above and none
of them is the page's Cause 1. The `native-shadow` row is the largest and is a
different question entirely — whether those 736 sites should reach their native
through something cheaper than the leaf-native site cache — which belongs to the
native-funnel floor work, not here.
