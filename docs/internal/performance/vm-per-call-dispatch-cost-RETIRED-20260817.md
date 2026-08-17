# The VM-wide per-call dispatch cost — RETIRED 2026-08-17

| | |
|---|---|
| **Status** | RETIRED — every item it left open is answered, and the residual it named has a mechanism, a count and a named fix class |
| **Opened** | 2026-08-13; retired 2026-08-13, **reopened 2026-08-17** as the named residual of live netty failures |
| **Closed by** | `perf/per-call-dispatch-residuals-20260817` |
| **Measured effect** | **-4% to -9% CPU** on `BigEndianHeapByteBufTest` (two ABBA rounds, neither overlapping); **-7.4%** on the page's own `MessageDigest.update(byte)` specimen, with the leaf rung as an unmoved control |

The page was reopened because it was the stated cause of three netty classes
over the suite's 180 s per-class cap, and because its own §3 conclusion — that
a percentage in a flat profile of this VM is a lead rather than a quantity —
had acquired one counter-example. Both are now settled, and the way they were
settled is the reusable part: **the page's central question was never
answerable from a profile, because the thing to count was not a symbol.**

## 1. What the per-call cost actually is

§1 counted `jit_entries` — 826 M on `AdaptiveByteBufAllocatorTest` — and §2
profiled the symbols they land in. Nothing said WHICH helper those entries are
or which arm answers them. `CRATONVM_DBG=mic-prof` now prints a `[DISP_CENSUS]`
line that does. On `BigEndianHeapByteBufTest`, 414/414:

| | count | share of `jit_invoke_dispatch` |
| --- | ---: | ---: |
| `kind_static` | 107 874 082 | **95%** |
| `kind_special` | 5 475 486 | 4.8% |
| `kind_virtual` + `kind_interface` | 289 | ~0 |
| `out_dcache` — a compiled callee from a thread-local cache | 111 543 628 | **98.4%** |
| `out_site_native` | 1 507 946 | 1.3% |
| `out_tail` — reaches `invoke_or_native` | 259 051 | **0.23%** |

Read the last two rows against §2's two levers. **Lever 2 — the native-registry
probe on every invoke — is 0.23% of this path, counted, not estimated.** §2.1
predicted ~1.5% of CPU from arithmetic and declined to build it; the count says
the arithmetic was generous. Lever 2 is closed.

Lever 1 is not what §2 thought either. The entries are not bookkeeping around a
call that had to happen: **98.4% of them are a compiled Java callee, already
compiled, reached through a Rust helper on every single call.** The helper's
whole job at those sites is to look up an entry pointer it already has cached
and then call it.

## 2. Why the helper is on that path at all

`DIRECT_CALLEE_BIND_HITS`/`MISSES` (`CRATONVM_DBG=jit-method-stats`) answer it,
and the answer is compile ORDER, not any property of the call:

> A statically bound call site is offered a direct `CALL` **exactly once**,
> while its caller is being compiled. If the callee is not compiled at that
> instant, `callee_compiler` hands back nothing, the site is bound to
> `jit_invoke_dispatch`, and nothing ever revisits the decision. The callee
> then compiles moments later — which is exactly what `out_dcache` counts.

Measured on the same class:

```text
[cratonvm] direct callee binds: 666 bound, 1394 left on the dispatch helper
```

**68% of the statically bound sites that asked for a direct target got
nothing**, and those 1 394 sites are what produce the 111.5 M helper
dispatches. That is a count, so unlike everything in the original §2 it does
not move with the host's load.

That names the fix class — re-binding a call site once its callees are live,
by recompiling the caller or by patching the site — and it is JIT codegen work,
deliberately not attempted here. It is the honest successor to this page, and
unlike this page it is stated as a mechanism with a count rather than as a
share of a flat profile.

## 3. The five round trips that were removed

Each removes a *round trip* — a lookup, a lock, a contended line — which is the
bar §3 sets, rather than shaving instructions off a path that runs.

1. **`JitMICSlot::redefine_epoch` was an unconditional `swap`.** A locked RMW on
   the slot's first eight bytes on every helper dispatch, storing a value that
   was already there on every run in which nothing is redefined — and dirtying
   the line the inline machine-code cascade loads at offset 0. Now a load, with
   the `swap` only when the epoch moved.

2. **Every cached compiled dispatch re-derived a keep-alive it already held.**
   `try_call_compiled_entry_reentrant` resolved its `Arc<CompiledMethod>`
   through `pin_jit_code_range_owner`: an `ArcSwap` load, a binary search over
   every registered code range, a `Weak::upgrade` CAS and the matching drop —
   for an artifact the dispatch cache holds in its `RetainedCode` field for
   exactly that entry pointer. The three cached sites clone it instead.
   Engagement: **`owner_reuse=111 564 628` against `registry_pins=37 484`** —
   99.97%. `CRATONVM_JIT='-cached-entry-owner-reuse'` restores the old path on
   one binary.

3. **The cache for exception-table callees was consulted after the work it
   exists to avoid.** Such a callee is barred from the MIC and the PIC, so
   nothing writes its slot's class id and every call is a "miss" forever;
   `VIRTUAL_DISPATCH_CACHE` serves those calls and the consult sat behind
   `record_miss` and `virtual_dispatch_target_cached`. Hoisted to the
   `cached_cid` load, ahead of the megamorphic PIC probe as well.

4. **The native funnel reached the thread-state cell three times per call.**
   `current_state()` to learn what to restore, `record_transition` to record
   `NativeRunning`, and `record_transition` again from the guard's `Drop` —
   10.8-14.4 ns of a 29-36 ns funnel, on **every native call in the VM**, from
   the interpreter and from compiled code alike. `enter_native_state` does it in
   one access; see §4.

5. **A declined compile probe was re-run on every call.**
   `probe_returned_none` read **6 664 927** on the allocator class — every
   entryless MIC hit in the run — for 9.47e9 cycles of `cyc_compile_probe`,
   ~3.5 s of a 461 s run re-asking an answered question. `MIC_COMPILE_DECLINED`
   memoizes the refusal per `(site, receiver class)` and re-probes once every
   1024 calls, so a transient refusal still retries. After: **2 424**, a 32x
   reduction, with `probe_memo_skips=74 469` accounting for the difference.

### What they are worth

**Read the counters and the in-run controls first.** They are immune to this
host, and the class-level clock is not: the SAME baseline binary measured
52.3 s, 56.9 s, 67.8 s and 72.5 s of CPU on this class during one afternoon —
a 39% spread on a fixed configuration, which is larger than the effect being
measured.

*Load-immune — engagement and refusal counts, one run each:*

| | before | after |
| --- | ---: | ---: |
| dispatches reusing the owner they already held | — | **111 564 628** (against 37 484 registry pins — 99.97%) |
| compile probes that ran and were refused | 76 985 | **2 424** (32x, with `probe_memo_skips=74 469`) |
| `[DISP_CENSUS] out_tail` — reaching `invoke_or_native` | 259 051 | 255 124 (0.23% either way; lever 2 was never the path) |

*Load-immune — the thread-state pair against its own control, SAME run, four
passes:* `current_state()` + 2x `record_transition` **9.2 / 8.1 / 11.0 / 9.9 ns**
against the same pair as one `NativeStateSpan` **4.0 / 4.3 / 3.7 / 3.4 ns**.

*Class level, three ABBA rounds on `BigEndianHeapByteBufTest`, CPU (user+sys),
order A B B A A B, 414/414 in every arm of every round:*

| round | baseline | fixed | |
| --- | --- | --- | ---: |
| items 1,2,3,5 | 70.05, 65.65, 67.82 | 64.10, 61.48, 59.23 | **-9.2%**, no overlap |
| all five, round 1 | 52.28, 67.47, 72.48 | 50.25, 56.23, 57.13 | *discarded* — the host drifted 39% across the round |
| all five, round 2 | 56.65, 56.12, 56.85 | 54.66, 55.25, 52.58 | **-4.2%**, no overlap |

**-4% to -9%** is the honest range: the two rounds whose arms do not overlap.
The middle round is reported and discarded rather than dropped silently — its
-14.9% is the number a single un-paired pair would have produced, and it is the
one number here that is certainly wrong.

**And on `AdaptiveByteBufAllocatorTest` — the class the page was reopened for —
it is worth nothing measurable.** ABBA, 127/127 in every arm:

| arm | runs (CPU) | mean |
| --- | --- | ---: |
| baseline | 269.07, 271.85 | 270.46 s |
| fixed | 267.97, 279.71 | 273.84 s |

Fully interleaved, fully overlapping, and the fixed mean is *higher*. That is a
null result and it is the most useful row in this section: the five round trips
came off the helper, and this class does not care, because **its cost is
entering the helper at all** — 259.5 M times. See §7 and the successor page.
(Both arms now run this class at 261-273 s rather than the 419-461 s earlier
records quote. That is the host, not a fix: nothing here can be worth 40%.)

The page's own specimen, `probes/NativeFunnelFloorProbe.java`, six arms in the
order A B B A A B on the Windows box with all five fixes in, every rung in the
same process:

| rung | baseline (3 runs) | fixed (3 runs) | |
| --- | ---: | ---: | ---: |
| `MessageDigest.update(byte)` | 118.21 / 117.40 / 117.37 | **106.29 / 111.52 / 108.92** | **-7.4%** |
| `System.identityHashCode` | 92.01 / 92.40 / 109.22 | **87.89 / 86.83 / 88.38** | **-10.4%** |
| `AtomicInteger.get` — **LEAF** | 86.62 / 84.93 / 87.63 | 87.21 / 86.22 / 91.56 | — |
| control: plain Java call | 8.64 / 8.56 / 8.32 | 8.50 / 8.82 / 11.04 | — |
| control: no call | 1.49 / 1.52 / 1.52 | 1.61 / 1.90 / 1.90 | — |

Both non-leaf rungs separate with no overlap — the worst fixed run beats the
best baseline run on each. **The leaf rung is the attribution**, and it is not a
control chosen after the fact: `safe_native_call_leaf` does not record a
thread-state transition at all (see its doc table), so a fix to that pair
CANNOT move it, and it does not. The two rungs that pay the transition move; the
one that does not, does not.

## 4. The floor, decomposed

The page quoted ~150-210 ns per registered native call from compiled code and
never split it. The in-tree step profiles do (`jit_native_dispatch_profile` and
`native_funnel_profile`, both `#[ignore]`d, both run on the Windows box):

**The funnel, priced against its own controls in one run**
(`native_funnel_profile`, four passes, read the last):

| step | before | after |
| --- | ---: | ---: |
| the bare callback, no funnel at all | 0.3 | 0.3 |
| `safe_native_call`, no arguments | 27.4-29.0 | 19.7-22.3 |
| `safe_native_call`, one object argument | 34.8-36.3 | 22.5-26.2 |
| `safe_native_call`, four arguments | 41.1-42.8 | 27.6-29.0 |
| `safe_native_call_prevalidated`, one object argument | 35.4-36.2 | 23.3-24.0 |
| **`current_state()` + 2x `record_transition`** | **10.8-14.4** | 8.1-11.0 |
| **… the same pair as one `NativeStateSpan`** | — | **3.4-4.3** |
| `heap.load_and_forward(obj)` | 6.3-6.6 | 4.5-4.7 |
| the two `INLINE_NATIVE_ARGS` scratch arrays | 3.6-3.8 | 2.8-2.9 |
| `young_spill_pressure()` | 2.3-2.4 | 1.7-2.1 |
| `catch_unwind` around the callback | 1.5-1.7 | 1.3-1.4 |
| `current_state()` **alone** | 0.9-1.1 | 0.6 |
| `native_diag_mask()`, the pin push, the STW probe, the JNI drain, `native_oom` | ≤ 0.7 | ≤ 0.6 |

**Read the two middle rows, not the two columns.** The columns are separate
runs on a shared box and every row moved, which is the ±20% §6 warns about; the
two middle rows are the OLD pair and the NEW pair measured against each other
in the SAME run, and that is the only comparison here immune to the host:
**9.9 ns against 3.4 ns, 2.6-2.9x**, or about 6.5 ns off every native call in
the VM.

The thread-state trio was 35-40% of the whole funnel — larger than
`catch_unwind`, the pinning, both GC probes and the diagnostic mask put
together — and `current_state()` alone at 0.9 ns is what says the cost was the
*repetition*, not the read: the funnel reached `SELF_CELL` three times per call.
It now reaches it once. `thread_state::enter_native_state` records
`NativeRunning` and hands back a `NativeStateSpan` that restores through a raw
pointer to the same cell, which this thread's TLS handle and the census registry
both keep alive for longer than any native call can last. The old trio stays in
the profile as the control rung beside the new one, because "the new one is
faster" is only a claim next to it.

**The dispatch preamble around it** (`jit_native_dispatch_profile`, a different
run — read the shape, not a cross-run subtraction; the same box put
`safe_native_call_prevalidated` at 64-94 ns in that pass and 35-36 ns in the
one above, which is the ±20% §6 warns about):

| step | ns |
| --- | ---: |
| helper preamble, site-cache hit and return, end to end (`Thread.currentThread`'s own arm) | 11-16 |
| `forward_jit_reference_args` | 10.1 at one reference, **25.4** at two |
| `decode_dispatch_values_into` | 5.6 at one, **21.4** at two |
| `heap.is_object_address` / `class_id_of` | 3.9 / 4.4 |
| `NATIVE_SITE_CACHE` probe (hit), `class_id_or_name_was_redefined` | 1.3 / 1.6 |
| `coerce_native_return`, `record_invocation`, `jit_site_key`, `note_jit_boundary` | ≤ 0.8 each |

So the floor is the funnel plus a **per-reference-argument slope** that appears
twice — once in `forward_jit_reference_args` and once in the decode — and not
the preamble's memo probes, which are all ≤ 1.6 ns. Two consequences worth
carrying forward:

* **The page's "clean single-call specimen" is not clean.** `md_update_byte`
  can re-enter Java (`ctx.invoke_virtual`, when the receiver is not ours) and
  appends to a growing accumulator, so it can never claim leaf and its cost is
  not all dispatch. `System.identityHashCode` is the cleaner non-leaf rung and
  `AtomicInteger.get` the cleaner leaf one; both are in the probe.
* **Widening the leaf set is the lever on this floor**, and it is an audit
  against `NativeMethodRegistry::set_leaf`'s four-part contract, one native at a
  time — not a change to the funnel.

## 5. The finding that is larger than anything above

`probes/NativeFunnelFloorProbe.java` carries two Java-callee rungs that differ
only by a never-taken `try`/`catch` in the callee. ABBA, ONE binary, ONE gate:

| rung | barred (default) | `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1` |
| --- | ---: | ---: |
| interface call, callee has no exception table | 21.64 / 21.18 | 19.76 / 19.45 |
| interface call, callee has `try`/`catch` | **221.72 / 218.61** | **19.30 / 18.55** |

**11.4x for the presence of an exception table in the callee**, with the control
rung unmoved. The gate is a correctness bar, not an oversight — the inline
machine-code cascade `CALL`s the raw entry, and nothing on that route can send
the callee's deopt sentinel through the callee's own table — so it cannot
simply be flipped. Lifting it is codegen work and belongs to the exception-table
record, which now has the size and the one-flag repro it never had.

Note what this is NOT: it is not a percentage in a profile, and no profile could
have produced it. It is the same shape as the `VarHandle` counter-example the
reopened page cited — a route that should not be taken, found by an A/B on a
gate rather than by attribution.

## 6. The questions §2.1 and §4 asked, answered

* **`push_entry_full`'s per-entry cost.** Superseded by §1: the entries are
  612 M *helper calls* on the allocator class (352 M `jit_invoke_virtual_mic`
  plus 259 M `jit_invoke_dispatch`, against 665 M `jit_entries`), and the
  question is not what the bookkeeping costs but why the helper is entered.

* **`cache_hits=0 (0.0%)` in every run — "the cache as built cannot hit".**
  **Structural, and correct.** `note_jit_boundary()` has 68 call sites, one at
  every Rust-JIT crossing, and `scan_active_jit_frames` is only ever REACHED
  through a crossing. A hit therefore requires two scans inside one crossing,
  which is not something the VM does. 0% is the right answer, and `band_words=0`
  says it is not a cost either. **No code change**: churning a GC root-scan path
  for zero measurable gain is precisely what §3 warns against.

* **`resolve_id_by_key=0` — "where a cheap version of lever 2 would start".**
  Closed as inapplicable rather than unreached. `NativeCallSite` is ONE STATIC,
  ONE TRIPLE by construction, so it cannot serve a dispatch chain whose triple
  varies per call; the mechanism is not unwired, it does not fit. What does
  serve those calls is `NATIVE_SITE_CACHE`, and the census shows it engaged
  (`out_site_native` 1.5 M, `mic_site_native` 5.9 M). Separately, `find` already
  carries the class prefilter and its 91%-negative quirks arm short-circuits on
  a clean descriptor with no allocation, so the 13.2 M negatives are ~0.1 s of a
  461 s run.

## 7. What this does NOT close

**The three netty classes are still over the 180 s cap.** They were the reason
the page was reopened, and they remain over it: `AdaptiveByteBufAllocatorTest`
is 127/127 in 461 s on the baseline, and nothing here is worth more than the
~9-10% the ABBA measured. Being honest about that is the point of retiring the
page rather than leaving it open:

* what the page was, was a *characterisation* — "the per-call cost is the whole
  answer" — with two levers it had measured and declined;
* what replaces it is a *mechanism with a count*: 1 394 statically bound call
  sites that could not bind a direct callee, producing 98.4% of the dispatch
  helper's traffic, because binding is decided once at the caller's compile and
  the callee is not compiled yet at that instant;
* and a *second, larger one*: 11.4x for an exception table in the callee, with a
  one-flag repro.

Neither of those is this page's to build — one is call-site re-binding, the
other is the inline cascade's exception routing, and both are JIT codegen. They
are filed together, with their counts and their one-flag repro, as
`known-issues/perf/a-compiled-call-goes-out-to-rust-two-causes-20260817.md`,
which is this page's successor. A page whose every open item is answered and
whose residual is a named mechanism owned by a live page belongs in `internal`,
not in `known-issues`. **A page that says "this is the VM's per-call cost, and
here are two levers not worth building" is not a bug report; it was a
measurement record all along.**

## 8. What did NOT convert — the original §3, unretracted

Nothing in the reopened page's §3 is withdrawn. The three changes it recorded
still measured what it said they measured, and its rule still holds for what it
covers: **size a lever by building it and measuring CPU, never by summing
profile lines.** What this branch adds is the other half of that rule, which
the `VarHandle` fix and §5 above both demonstrate:

> A profile can only rank the work a path does. It cannot see a path that
> should not be taken, a cache that is never read, or a question asked once per
> call that was answered on the first one. Those are counters — and every one of
> the five fixes in §3 was found by a counter or a step profile that did not
> exist, or had never been run, when the page was written.

## 9. Repro

```bash
cd apps/netty-suite-runner
CLS=io.netty.buffer.BigEndianHeapByteBufTest

# the census: which helper, which arm, and the bind counters that say why
CRATONVM_DBG=mic-prof,jit-scan-prof,jit-method-stats <cratonvm> \
    --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner $CLS \
  2>&1 | grep -E "DISP_CENSUS|MIC_PROF|JIT scan prof|direct callee binds"

# the A/B for item 2, on ONE binary
CRATONVM_JIT=-cached-entry-owner-reuse <cratonvm> ... CratonRunner $CLS

# the 11x, and the gate that shows it
<cratonvm> --java-home <jdk25> --Xmx 1500m -cp <out> NativeFunnelFloorProbe 1000000 20
CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=1 <cratonvm> ... NativeFunnelFloorProbe 1000000 20

# the funnel, step by step
cargo test --release -p cratonvm-vm --lib jit_native_dispatch -- --ignored --nocapture
cargo test --release -p cratonvm-vm --lib funnel_cost_breakdown -- --ignored --nocapture
```

## 10. Measurement hygiene — unchanged, and it earned its keep again

This box runs many concurrent agents; load moved between 5 and 31 during this
work. Every timing above is ABBA-interleaved on ONE binary with an in-process
control, and every structural claim is a COUNT, which is immune to load
outright. Two things this session would have got wrong without that discipline:
the same binary measured `MessageDigest.update(byte)` at 152.88 and 307.30 ns
in two consecutive runs, and the first netty pair suggested -16% where the full
ABBA says -9.2%.
