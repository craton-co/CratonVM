# `java.util.concurrent` primitives after the compile refusals — every residual discharged

## Status
**CLOSED, 2026-09-01.** Opened 2026-08-28 as the residual of two pages that
closed the same week. It carried one fix ("Where to look first" #1), one
missing diagnostic (#2), one unanswered question about composition's
amortisation, and one measurement it did not trust. All four are discharged:

| item | disposition |
|---|---|
| #1 `VarHandle.get` on a reference field is unbound, 77.0 ns and 1.9x an `int` read | **FIXED** — bound in `267549501`; 140.6 -> 64.3 ns cpu, **2.18x**, and the 1.9x over the `int` read is now **0.99x** |
| #2 no native profile of composition with everything compiled | **TAKEN** — and it says composition is **4.9 %** compiled code; see "The profile" |
| "whether that is warmup, thread scaling or both is NOT established" | **ANSWERED** — warmup. Thread scaling costs CratonVM 1.09x over a 24x thread increase; see "The two sweeps" |
| the table "needs a clean idle-box re-measurement" | **SUPERSEDED** — the probe now reports per-row CPU time, which does not need one; see "Why the idle box stopped being required" |

What this page was ABOUT — that these primitives are 12-37x — is not fixed and
was never going to be by one bind. What remains is narrower than this page and
has a different shape, so it is a new page rather than an open section here:
[`completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`](completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md)
— which closed on 2026-09-02, discharging all three of its own residuals and
measuring composition **1.19x** faster.

## What was fixed

`VarHandle.set` was bound on 2026-08-24 and `compareAndSet` on 2026-08-28. This
page's #1 was the third and last access direction, and it named the obstacle
correctly: `unbox_poly_return_checked`'s W6-1 rule turns *a boxed primitive
reaching a non-`Object` reference return* into a `WrongMethodTypeException`,
and that rule reads the CALL SITE's descriptor, which a baked direct call has
no `JitInvokeInfo` to carry.

The obstacle was real and the conclusion drawn from it was not, because it
priced only the COLD arm.

**The fast arm could never lose W6-1.** `varhandle_instance_field_read_bits`
refuses unless the variable's own kind agrees with the site's, so a reference
site over a PRIMITIVE variable — W6-1's entire fire set — is declined there.
And the identical refusal already governed `try_varhandle_instance_field_read`,
the funnel's own copy of this read, which has served reference returns since it
was written and returns raw bits WITHOUT reaching `unbox_poly_return_checked`
at all. Compiled code's reference reads were already outside W6-1 before this
change; binding them moved their cost, not their semantics. **The page reasoned
about the rule and not about which of the two routes actually reaches it.**

**The cold arm keeps W6-1 by classifying the site instead of carrying its
descriptor.** A boxed primitive is assignable to exactly fourteen reference
types — `java/lang/Object`, the five shared wrapper supertypes and the eight
wrappers themselves — so a reference site is one of three things, and only the
third would have needed the descriptor:

* `Ljava/lang/Object;` (`REF_OBJECT`): the erased stand-in IS the real
  descriptor, and no box is refused at an `Object` return;
* one of the other thirteen (`VARHANDLE_BOX_ACCEPTING_RETURNS`): whether a box
  satisfies it depends on WHICH wrapper arrived (`Integer` at a
  `java/lang/Number` site is fine, `Character` is not), so the site is **not
  bound**;
* anything else (`REF_STRICT`): NO box is assignable, so "the cold arm produced
  a box" is a W6-1 fire needing no further information.
  `varhandle_strict_reference_return_check` raises it.

`CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT=0` reverts, and is deliberately a
different switch from `CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS`: turning the
whole read bind off would move the primitive rows too and price the wrong
change.

## The measurement

`HibfixVarHandleProbe`, `-Dprobe.iters=2000000`, eight INTERLEAVED reps per
arm, ONE binary with the kill switch as the only difference. Host load 26.8-31.7
throughout. **Per-row CPU time**, for the reason in the next section:

| operation | bind ON | bind OFF | OFF/ON | HotSpot | x HS |
|---|---:|---:|---:|---:|---:|
| `VarHandle.get` reference | 60.6-67.3 (**64.3**) | 138.1-142.3 (**140.6**) | **2.18x** | 5.0 | 12.7x |
| `VarHandle.compareAndSet` reference † | 169.8-177.1 (**175.6**) | 246.4-273.3 (**257.2**) | 1.47x | 10.4 | 16.9x |
| `VarHandle.get` int (control) | 59.1-65.7 (**65.0**) | 61.0-68.0 (**62.9**) | 0.97x | 5.8 | 11.2x |
| `VarHandle.set` reference (control) | 89.2-91.7 (**89.8**) | 86.9-90.6 (**89.5**) | 1.00x | 8.3 | 10.8x |
| `VarHandle.compareAndSet` int (control) | 154.2-162.1 (**160.3**) | 154.2-161.5 (**158.9**) | 0.99x | 11.2 | 14.3x |
| `AtomicReference.compareAndSet` (control) | 334.0-364.9 (**344.4**) | 318.5-367.2 (**342.9**) | 1.00x | 10.9 | 31.7x |
| `AtomicInteger.incrementAndGet` (control) | 5.5-6.2 (**5.8**) | 5.5-6.0 (**5.9**) | 1.01x | 8.8 | **0.7x** |
| plain field store (control) | 12.9-13.4 (**13.3**) | 11.9-13.5 (**13.2**) | 1.00x | 6.2 | 2.2x |

> **The `HotSpot` and `x HS` columns are SOFT and are here for orientation
> only.** The A/B columns are one binary with one switch, so the host cancels
> out of them; the HotSpot column is a different process on the same busy host
> and nothing cancels. Its own control is the proof: `AtomicInteger.increment`
> reads 8.1, 8.5, 8.7, 8.7, 9.0, 10.4, 23.6, 24.5 ns cpu across the eight reps
> — a 3x spread on a row that cannot have moved. Read the medians as an order
> of magnitude, not as ratios; the numbers this page CONCLUDES from are the
> OFF/ON column and the engagement counts.

† still a COMPOSITE — a `get` and then the CAS — and it moves for the read
inside it. Subtracting the matching `get` row leaves **116.6** ns OFF against
**111.3** ns ON for the CAS alone, i.e. unmoved to within 5 %. That arithmetic
is the reason to believe the 2.18x is the read and not the box.

**Six controls unmoved is what makes this the change and not the host.** The
row this page opened on — "77.0 ns against 41.3 for the same read of an `int`,
and the 1.9x is its measured price" — reads **64.3 against 65.0, i.e. 0.99x**.
A reference read now costs what a primitive read costs.

### Re-measured after merging dev

`dev` moved 75 commits between the branch point and the merge, so the table
above was re-taken on the MERGED tree — the rule that a 3/3 pass can become a
2/2 fail on same-day dev. Five interleaved reps per arm, load 59.1-61.0, and
the verdict is unchanged:

| operation | bind ON | bind OFF | OFF/ON |
|---|---:|---:|---:|
| `VarHandle.get` reference | 64.1-68.2 (**66.1**) | 134.3-141.8 (**140.2**) | **2.12x** |
| `VarHandle.compareAndSet` reference † | 171.6-178.0 (175.7) | 255.8-268.1 (263.1) | 1.50x |
| the six controls | — | — | 0.97-1.04x |

(2.18x before the merge, 2.12x after, at twice the load.) The merged tree also
passes the regression suite **121/121, 0 harness-blindness flags**, and
`RJitVarHandleRefRead` is byte-identical to HotSpot in both arms of the kill
switch.

### Engagement, which needs no quiet box

A count is immune to load in a way a timing is not, and this is the half that
says the compiled path actually changed. Same probe, both arms:

| | bind ON | bind OFF |
|---|---:|---:|
| `VarHandle read thin direct calls: served` | **8 000 000** | 4 000 000 |
| `of which VarHandle instance-field reads served directly` (the funnel fast path) | **0** | 4 000 000 |
| `compiled site-cached native dispatches (non-leaf)` | **2 198 017** | 6 197 017 |
| bound sites, singlepass/OSR | 0/**4** | 0/**2** |

Exactly 4 000 000 reads move off the funnel and 0 are declined, and the funnel's
own dispatch count falls by 3 999 000. Whole-probe CPU over six interleaved
reps: **2.22-2.31 s** ON against **2.53-2.72 s** OFF — non-overlapping, 1.17x.

## Why the idle box stopped being required

This page's first revision was measured on a busy box, got 9-114x and a wrong
diagnosis, and the page then insisted on an idle host and six interleaved runs.
That insistence was right and it is not affordable: this host has not been idle
once in this session (load 17-64), and it is somebody's daily machine.

The fix is not a quieter box, it is a better clock. `HibfixVarHandleProbe` now
prints a `cpu_ns_per_op=` column beside its wall column, from
`ThreadMXBean.getCurrentThreadCpuTime` (supported on CratonVM; the loops are
single-threaded, so the current thread's cpu time IS the row's cost). The
difference is not subtle — the SAME eight reps, same binary, same arms:

| | wall ns/op | cpu ns/op |
|---|---|---|
| within-arm spread, worst row | **3-7x** | under 10 % |
| `AtomicInteger.increment`, ON vs OFF | 0.62x | 1.01x |
| `VarHandle.get` reference, ON vs OFF | 2.46x | 2.18x |

Read the wall column and six of the eight controls appear to have moved. Read
the cpu column and none of them has. **A wall-clock row on this host is a
measurement of the other tenants.** The wall column is kept for continuity with
the tables above it; the cpu column carries the verdicts.

The same substitution rescues the composition numbers: this page's idle-box
19x (wall) and today's ~20x (cpu, at load 24-38) agree, which is the evidence
that cpu time transfers across load regimes and wall time does not.

## The profile — "Where to look first" #2

`perf record -F 999` on `HibfixComposeProbe2` (2 threads, 80 000 chains each)
with the current binary. **5 314 samples, and no symbol is above 3 %** — so a
top-N list answers nothing and the buckets below are the answer. The two worker
threads are symmetric and are summed:

| bucket | share |
|---|---:|
| **JIT-compiled Java code (the program itself)** | **4.9 %** |
| other VM runtime | 21.2 % |
| interpreter | 15.7 % |
| dispatch: invoke plumbing | 10.1 % |
| name-keyed lookup (string hash/compare) | 8.2 % |
| GC: barriers, forwarding, roots | 7.6 % |
| `VarHandle` CAS/write natives | 7.4 % |
| GC: heap-address validation | 7.1 % |
| GC: allocation | 6.0 % |
| libc / kernel | 5.3 % |
| field-access helpers | 3.4 % |
| type checks | 3.3 % |
| locks / monitors | **0.2 %** |

The largest individual symbols, per bucket, are
`compare_and_swap_field_shared` 5.39 %, `execute_frame_from_index` 4.71 %,
`ZObjectStarts::contains` 3.69 %, `is_object_address` 3.05 %,
`invoke_on_class_shared_inner` 2.39 %, `__memcmp_evex_movbe` 2.54 %,
`alloc_raw_tlab` 1.77 %.

This page said "leaves ~90 % of the chain unattributed". It is attributed now,
and the answer is that **there is nothing to find at the top** — composition is
uniformly expensive across the whole dispatch and GC support stack, which is
exactly why the CAS bind moved it 1.10x and this page's read bind moves it
1.00x. `locks / monitors` at 0.2 % also retires, for good, the family of
hypotheses this page inherited about contention.

### A correction this page owes

> `CRATONVM_DBG=jit-method-stats` on the composition probe reports
> `hot_but_stuck_in_interpreter=0` … so what follows is the cost of COMPILED
> code, not of the interpreter.

That is sound for the per-op probe and **not sound for composition.** The same
run that reports the zero also reports `8 distinct methods tracked, 6 ever
invoked` — the counter ranges over eight methods, so its zero says almost
nothing — and the profile puts **4.9 %** of samples in compiled code against
**15.7 %** in the interpreter. `hot_but_stuck_in_interpreter=0` means "no
TRACKED method was hot and uncompiled". It does not mean "the workload is
compiled", and on this workload it is not. A zero from an instrument whose
population is eight is the shape of a vacuous zero.

## The two sweeps — composition's amortisation

This page recorded that per chain CratonVM goes 11.0 -> 5.6 us as the workload
grows while HotSpot goes 0.58 -> 0.045-0.088, and said the arms that separate
warmup from thread scaling are cheap. They are; both changed at once between its
two rows (2 threads/160 000 chains against 24 threads/4.8 M), so the pair could
not attribute the difference. Held one at a time, CPU us per chain, 3 reps:

**A — total chains fixed at 480 000, threads vary:**

| threads | chains/thread | CratonVM | HotSpot | ratio |
|---:|---:|---:|---:|---:|
| 1 | 480 000 | 30.67 | 0.708 | 43x |
| 2 | 240 000 | 30.96 | 0.854 | 36x |
| 4 | 120 000 | 31.50 | 0.917 | 34x |
| 8 | 60 000 | 31.90 | 1.167 | 27x |
| 24 | 20 000 | 33.52 | 1.312 | 26x |

**B — 2 threads, total chains vary:**

| chains/thread | total | CratonVM | HotSpot | ratio |
|---:|---:|---:|---:|---:|
| 20 000 | 40 000 | 36.50 | 5.000 | 7x |
| 80 000 | 160 000 | 32.38 | 1.938 | 17x |
| 320 000 | 640 000 | 31.66 | 0.609 | 52x |

**It is warmup, and it is HotSpot's warmup.** CratonVM's per-chain cost is flat
in thread count (**1.09x across a 24x increase**) and nearly flat in workload
size (**1.15x across 16x**). HotSpot's is 1.85x WORSE at 24 threads and **8.2x
better** at 16x the work. So:

* CratonVM has no thread-scaling problem in composition. The narrowing of the
  ratio from 43x to 26x as threads rise is HotSpot degrading, not CratonVM
  improving — a fact that reads backwards from the ratio column alone;
* the page's "both amortise, HotSpot amortises several times harder" is
  confirmed and now attributed: CratonVM reaches steady state almost
  immediately at ~31 us/chain and stays there, while HotSpot keeps compiling
  down to 0.6. That points at inlining depth in a chain of tiny methods, not at
  locks, scheduling, or the primitives this page is named after — and it is the
  same shape as
  `devirtualizing a chain of tiny methods bought nothing`;
* the page's 64-125x for its 24-thread row was two variables moving together.
  Separated, neither is worth 64x on its own.

## Found on the way, and NOT fixed here

`VarHandle` accesses with a **null coordinate** answer instead of throwing:
`REF.get((Holder) null)` returns `null`, an `int` read returns `0`, and a `set`
silently does nothing, where HotSpot raises `NullPointerException` for all
three. It is pre-existing and unrelated to this page — it reproduces
identically with `CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT=0`, i.e. with every
access on the funnel — and closing it means touching every access mode and
every handle kind, so it is filed rather than folded in:
[`../../known-issues/jdk-only/varhandle-null-coordinate-answers-instead-of-throwing-20260901.md`](../../known-issues/jdk-only/varhandle-null-coordinate-answers-instead-of-throwing-20260901.md).

## Repro

Both probes are now TRACKED, under `apps/probes/`, beside their 86 siblings.
They were untracked in six worktrees, and this page's original Repro block
pointed at `apps/hibernate-reactive-suite-runner/` — a directory that is in no
commit, so the block could not be followed by anyone who did not already have
the files.

```bash
javac -d <out> apps/probes/HibfixVarHandleProbe.java apps/probes/HibfixComposeProbe2.java
# the A/B is one binary and one switch
CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT=1 cratonvm --java-home <jdk> -Dprobe.iters=2000000 -cp <out> HibfixVarHandleProbe
CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT=0 cratonvm --java-home <jdk> -Dprobe.iters=2000000 -cp <out> HibfixVarHandleProbe
# engagement, which is a count and does not need a quiet box
CRATONVM_INTRINSIC_STATS=1 cratonvm ... 2>&1 | grep "read thin direct"
```

Read the `cpu_ns_per_op=` column. Interleave the arms. Six reps is still the
minimum, and the controls are still what says whether the run is a measurement.

## Gates

`RJitVarHandleRefRead` is new and exercises all three site classifications HOT
(300 000 rounds, so both the single-pass and the OSR door bind), green
byte-for-byte against HotSpot in BOTH arms of the kill switch. `RJdkHandles`
already carried `String bogus = (String) intVarHandle.get(h)`, but runs it once
in a cold method, so it only ever tested the interpreter's route through the
funnel — the bind is a compile-time decision and needed a hot vector.
`threw=3000` is the row that matters: every hot `REF_STRICT` wrong-type read
raised from the bound helper's own cold arm.

Regression suite 118/119; the one failure is `RMapGcStress` hitting the
harness's own 120 s budget at load 29, which the harness diagnoses itself and
prescribes `TIMEOUT=600` for.

## Related

- [`varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`](varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md)
  — the first two access directions. Its closing note ("`CompletableFuture`
  composition is not VarHandle-bound") is confirmed a second time here, by a
  profile rather than by arithmetic.
- [`varhandle-compareandset-thin-direct-bind-FIXED-20260828.md`](varhandle-compareandset-thin-direct-bind-FIXED-20260828.md)
- [`completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md`](completablefuture-composition-force-interpreted-by-a-stale-forkjointask-blocklist-FIXED-20260827.md)
- [`completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`](completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md)
  — the successor. CLOSED 2026-09-02: all three of its residuals discharged,
  composition 1.19x. What is still open from this line is
  [`../../known-issues/perf/composition-native-callback-and-the-promotion-question-20260902.md`](../../known-issues/perf/composition-native-callback-and-the-promotion-question-20260902.md).
- [`../../known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`](../../known-issues/perf/interpreted-invoke-cost-350ns-20260825.md)
