# `CompletableFuture` composition is ~20x, and only 4.9 % of it is compiled code

## Status
**OPEN, opened 2026-09-01.** The successor to
`performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`,
which discharged all four of its own residuals and left this. Everything below
is a measurement taken on the binary at `267549501`; nothing here is inherited
prose.

The predecessor spent three revisions looking for the component that dominates
composition. **There is not one.** That is this page's entire content, and it
is a different kind of problem from the one the predecessor was written for.

## Severity
**MEDIUM-HIGH and broad, but not urgent.** `CompletableFuture` composition sits
under reactive Spring, hibernate-reactive and Vert.x. It is not a correctness
issue and it is not a cliff — the cost is flat and predictable. What makes it
worth a page is that the fix is structural, so it will not arrive by accident.

## The measurement

`HibfixComposeProbe2` — no scheduler, no locks, no cross-thread handoff, each
thread owning its futures end to end, `wrong=0` in every run. CPU time, because
the host of record has not been idle once (load 24-38 during these runs) and
wall clock there measures the other tenants; see the predecessor's "Why the
idle box stopped being required".

| | CratonVM | HotSpot | ratio |
|---|---:|---:|---:|
| 2 threads, 160 000 chains, whole-process cpu | 5.02-5.34 s | 0.22-0.33 s | **~20x** |
| cpu us per chain, 2 threads, 640 000 chains | 31.66 | 0.609 | **52x** |

The predecessor's idle-box wall-clock ratio for the same 2-thread shape was
19x. That the cpu ratio here is ~20x at load 24-38 is the evidence that cpu
time transfers across load regimes.

## The profile, and why it is the finding

`perf record -F 999`, 5 314 samples, 2 threads x 80 000 chains. **No symbol is
above 3 %.** The two workers are symmetric and are summed here:

| bucket | share |
|---|---:|
| **JIT-compiled Java code — the program itself** | **4.9 %** |
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
| locks / monitors | 0.2 % |

**Composition spends 4.9 % of its time running the program and 95 % supporting
it.** Every previous attempt on this workload looked for a hot component and
found a 5 %-shaped one, then measured a 5 %-shaped win — the CAS bind moved it
1.10x, the reference-read bind moves it 1.00x. That is not three failed
hypotheses, it is one structural fact seen three times.

Four clusters are individually worth naming, and each is a candidate:

1. **The interpreter, 15.7 %** (`execute_frame_from_index` 4.71 %, `execute`
   1.09 %, `execute_invokevirtual_cached` 1.05 %). See the caution below —
   `hot_but_stuck_in_interpreter=0` on this workload is a zero over a
   population of eight methods.
2. **Name-keyed lookup, 8.2 %** (`HashMap<&str,()>::contains_key` 1.24 %,
   `__memcmp_evex_movbe` 2.54 %, `NativeMethodRegistry::find_with_kind`
   0.70 %, `from_utf8` 0.40 %). String hashing and comparison on a dispatch
   path, per call.
3. **GC heap-address validation, 7.1 %** (`ZObjectStarts::contains` 3.69 %,
   `is_object_address` 3.05 %). The same pair that was 10.3 % of the netty
   `RefCnt` profile before the read memo went in; the memo took it off the
   `VarHandle` fast paths and this workload reaches it by other routes.
4. **The CAS native, 7.4 %** (`compare_and_swap_field_shared` 5.39 %). Already
   thin-direct-bound — 1 677 766 served, 0 declined — so this is the bound
   path's own cost, not a funnel miss.

## What amortisation says about where to look

Two sweeps, CPU us per chain, one variable at a time (the predecessor's two
rows moved both at once and could not attribute):

**Total chains fixed at 480 000, threads vary:** CratonVM 30.67 -> 33.52 across
1 -> 24 threads (**1.09x**). HotSpot 0.708 -> 1.312 (**1.85x worse**).

**2 threads, total chains vary 40 000 -> 640 000:** CratonVM 36.50 -> 31.66
(**1.15x**). HotSpot 5.000 -> 0.609 (**8.2x**).

**CratonVM does not amortise and HotSpot does.** CratonVM reaches steady state
almost immediately and stays at ~31 us/chain; HotSpot keeps improving by 8.2x
over the same range. Thread scaling is not the problem at all — CratonVM is
flatter than HotSpot there.

A chain of tiny JDK methods that costs the same on call one million and call
one is a workload that is **not being inlined**. That is consistent with the
4.9 % figure (if the bodies were inlined into one compiled region, more of the
time would be IN that region and less in per-call support), and with
`reference_devirtualizing_a_chain_of_tiny_methods_bought_nothing`: 438 sites
devirtualized moved the workload 0 %, because a chain of tiny methods is slow
for the calls being CALLS, and only inlining removes the frame, the spill, the
boundary note and the oop map.

## Where to look first

1. **Why is 15.7 % of a fully-warm composition run in the interpreter?**
   Establish it properly first: the tracked-method counter reports `8 distinct
   methods tracked, 6 ever invoked, 3031 total invocations` on a run that
   builds 160 000 chains, so it is not describing this workload. Find which
   frames `execute_frame_from_index` is running — JDK library methods that
   never tier, callees reached through `invoke_or_native` from compiled code,
   or deopts — before proposing anything. This is the largest single bucket
   after "other VM runtime" and the only one whose SIZE is currently unexplained.
2. **IR-tier inlining on this workload.** `CRATONVM_JIT_IR_INLINE=1` exists and
   was worth ~10x on the kfusion voxel read. Nobody has run composition with it.
   It is one run, and the amortisation sweeps above predict it is the lever
   that matters. Measure with `-Dprobe.chains=320000` so the arm is in the flat
   part of sweep B.
3. **Name-keyed lookup, 8.2 %.** Per-call string hashing on a dispatch path is
   a cost with a known shape; the question is which call sites reach
   `find_with_kind` and `invoke_on_class_shared_inner` with a name rather than
   a resolved id.

## What is already excluded, with the evidence

Do not re-derive these.

* **Locks and contention.** 0.2 % of the profile. The predecessor inherited a
  family of hypotheses here from `HibfixComposeProbe`, whose profile was ~45 %
  scheduler — but that probe used a `ScheduledThreadPoolExecutor` and this one
  deliberately does not.
* **Thread scaling.** 1.09x across 1 -> 24 threads, and HotSpot is worse.
* **`VarHandle` READ binds of any kind.** `VarHandle read thin direct calls:
  served=0 declined=0` on this workload — composition makes no compiled
  VarHandle read calls at all, so no read bind can reach it. This is why the
  2026-09-01 reference-read bind, which is 2.18x on the per-op probe, is 1.00x
  here (5.02-5.34 s cpu against 5.14-5.49 s, overlapping).
* **The `VarHandle` CAS funnel.** Bound; 1 677 766 served, 0 declined.
* **Compile refusals of the 2026-08-27 kind.** Both fixed, and restoring either
  with its kill switch still costs 2.19x and 2.88x, so the instrument works.
* **Java-frame profiling.** It attributes a whole native call to the Java frame
  that made it, and it produced "34 % in `tryPushStack`" once already, which
  sent three days into `VarHandle` primitives that were not the constraint.
  Use `perf` on the binary.

## Repro

```bash
javac -d <out> apps/probes/HibfixComposeProbe2.java
/usr/bin/time -f "cpu user=%U sys=%S wall=%e" \
  cratonvm --java-home <jdk> -Dprobe.threads=2 -Dprobe.chains=80000 -cp <out> HibfixComposeProbe2
perf record -F 999 -o compose.perf -- cratonvm ... HibfixComposeProbe2
perf report -i compose.perf --stdio --sort dso     # the 4.9 % line
```

Read CPU time, not wall. Report `/proc/loadavg` beside every row.

## Related

- `performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md` (internal)
  — the predecessor, and the source of every "already excluded" row above.
- [`interpreted-invoke-cost-350ns-20260825.md`](interpreted-invoke-cost-350ns-20260825.md)
  — the same interpreter, priced per invoke.
- [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
