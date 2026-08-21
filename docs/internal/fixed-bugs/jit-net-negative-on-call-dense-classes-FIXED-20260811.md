---
name: jit-net-negative-on-call-dense-classes-FIXED-20260811
description: FIXED 2026-08-11. The JIT's +83%/+52% CPU deficit on ZipContentTests was a leaked JMX owned-monitor set - jit_monitor_exit released the monitor and never retracted the ownership publish jit_monitor_enter made, so a linearly-scanned Vec grew to every object ever locked from compiled code and made monitorenter quadratic (54% of the run under perf, absent from the --nojit control). One missing retract on one of six exit sites. The JIT arm went 144.48s -> 59.17s CPU (-59.0%, 3/3) and now BEATS --nojit by 36.5%. The page's own diagnosis - the never-publishing inline cache and the dispatch funnel - was measured after the fix at ~6% with identical call counts, and was never the cost.
metadata:
  type: fixed-bug
  area: jit, jmx, monitors, throughput
---

# The JIT was a net negative on call-dense classes — a leaked JMX monitor set

**FIXED 2026-08-11.** Split out of
`internal/fixed-suite-bugs/springboot/zipcontenttests-bytebuffer-accessor-call-cost-RETIRED-20260810.md`,
filed 2026-08-10 as `jit-net-negative-on-call-dense-classes-20260810` under the
public known-issues tree, and moved here on close.

## The result

`org.springframework.boot.loader.zip.ZipContentTests`, Azure Linux, `-Xmx 2g`,
per-process CPU, three rounds with the arm order rotated per round:

| round | JIT before | JIT after | `--nojit` after |
|---|---:|---:|---:|
| 1 | 143.42 | 53.32 | 85.47 |
| 2 | 144.80 | 52.74 | 106.06 |
| 3 | 145.23 | 71.46 | 88.19 |
| **mean** | **144.48** | **59.17** | **93.24** |

The JIT arm is **-85.31 s, -59.0%, 3/3**. Against `--nojit` it has gone from
**+52% to -36.5%**: on the class this page was filed about, the JIT now wins.
29 tests / 0 failed on every arm of every run.

## What it was

`jit_monitor_enter` reaches `complete_jmx_monitor_enter` by way of
`monitor_enter_blocking`, so a compiled `monitorenter` appends its object to the
thread's `ThreadRegistry::jmx_locked_monitors` exactly as the interpreter's
does. `jit_monitor_exit` released the monitor and stopped there.

That list is three things at once: what `ThreadMXBean.getLockedMonitors()`
reports, a GC root set, and — the part that costs — a `Vec` that is
**membership-scanned linearly on every acquisition**. A publish with no matching
retract therefore does not leave one stale slot. It grows the list to every
distinct object the thread has ever locked from compiled code and makes
`monitorenter` O(that), while pinning every one of those objects as a root.

`ZipContentTests` locks constantly — `Inflater`, `Deflater`, `CleanerImpl$CleanableList`
and `Inflater$InflaterZStreamRef` are all synchronized, and the callee census
counts 129 780 calls to `CleanableList.insert` alone.

A sixth site was leaking the same way: the stack-overflow bail in
`invoke_cached` exits the synchronized-method monitor it had just acquired
without retracting it.

## How it was found, and why the page could not find it

`perf record` on one JIT run and one `--nojit` run, flat profile, no
call-graph — five minutes of wall clock:

| symbol | JIT | `--nojit` |
|---|---:|---:|
| `ThreadRegistry::complete_jmx_monitor_enter` | **36.34%** | absent |
| `ThreadRegistry::remove_jmx_locked_monitor` | **17.87%** | absent |
| `Arena::alloc` | 8.33% | 3.24% |

Neither scan function appears above 0.5% in the `--nojit` control, which is what
makes it a JIT-only cost rather than a property of the workload. `Arena::alloc`
is inflated by the same defect from the other side: the leaked entries are roots,
so their objects are never reclaimed.

The page had four mechanisms, each killed by a measurement, and then a fifth it
believed. The fifth was wrong too, and the reason is worth keeping: **every
counter it used was inside the funnel it suspected.** `mic_calls`, `disp_calls`,
`hit_noentry`, `pub_published` and the `CycGuard` cycle totals all measure the
dispatch helpers, and the dispatch helpers were where the JMX scan was being
*paid from* — so the funnel looked expensive because something underneath it
was. No counter in that family can distinguish "this helper is slow" from
"something this helper calls is slow", and adding more of them cannot either.
A profiler answers it in one run, and the first one taken named the defect
outright.

The same profile is also what retires the page's own prescription. Post-fix, on
the same workload:

| | before | after |
|---|---:|---:|
| `mic_calls` | 26 580 702 | 26 551 827 |
| `disp_calls` | 15 252 970 | 15 239 903 |
| `hit_noentry` | 5 534 550 | 5 503 541 |
| `pub_published` | 0 | 0 |
| `cyc_mic_total` | 609.6e9 | **104.0e9** |
| run CPU | 254.83 s | **53.75 s** |

**The call counts are unchanged and the cost collapsed.** The funnel was never
the bill.

## The fix

The release-and-retract pair is now ONE function,
`vm_exec::monitor_exit_and_retract_jmx`, and the sites that spelt it out by hand
call it: the interpreter's `monitorexit`, the frame-pop implicit exit,
`JitSynchronizedMonitorGuard::drop`, the native `monitor_exit`, the
stack-overflow bail, and the JIT helper that was missing it.
`SynchronizedMethodGuard::drop` keeps its own copy and says why (it holds the
two halves, not a `SharedVm`). The `holds` re-check stays inside the shared
function: a re-entrant acquisition is still held after the inner release, and
retracting there would under-report a monitor the thread really does own.

This shape had already been found and fixed once, at the synchronized-method
guard, where it was 14.9% of a Tomcat webapp deploy — and that fix's own comment
predicts this recurrence almost word for word. Two occurrences of one shape is
why the pairing is a function now and not a convention.

Tests (`vm/src/jit/helpers.rs`, `jit_monitor_jmx_pairing`) drive the real
`extern "C"` helpers with the JIT thread pointer installed: one balanced pair,
64 distinct objects, and a re-entrant pair. The first asserts the publish IS
visible while the lock is held, so a helper that does no bookkeeping at all
cannot pass them.

## The page's reproducer, run at last

`probes/JitEntryFloorProbe.java` was filed as "**Not yet run under CratonVM**".
It runs now, and it did not run before for a reason worth recording: its single
measurement row is a `System.out.printf(Locale.ROOT, …)`, and
`PrintStream.printf(Locale, String, Object[])` had **no native and printed
nothing at all** — no wrong string, no exception, no output, beside a working
no-locale overload. The probe ran green, printed its header and its `sink=`
guard line, and omitted the number. Registered; see
`native_printf_locale` and the `fix(io):` commit.

With that closed, on the fixed binary (min of 3, ns per call):

| arm | ns/call | what it is |
|---|---:|---|
| both compiled | 3.50 | a JIT→JIT call |
| crossing (`CRATONVM_JIT_DENY=JitEntryFloorProbe.driver`) | 183.59 | one interpreter→JIT entry + return |
| `--nojit` | 224.55 | no crossing at all |
| HotSpot control | 0.38 | fully inlined |

The probe's own decision rule was "if `crossing` is slower than `interpreted`,
compiling a short method that an interpreted caller invokes is a net loss."
**It is not**: 183.59 vs 224.55, an 18% win. The crossing is nonetheless
expensive in absolute terms — ~52x a JIT→JIT call — and that number is now
measured rather than assumed. It is a real optimization target; it was not this
page's defect.

## What is left, priced

Kept because the next person will otherwise re-derive them.

**`pub_published = 0` is still true.** The megamorphic inline cache never learns
a callable entry: `probe_returned_none` accounts for 100% of `pub_probe_none`
(the split landed for this page — the counter used to collapse four causes,
three of which never run a probe at all). The refused callees are dominated by
things that can never compile — `Unsafe.putReference`, `Inflater.inflate`,
`Deflater.deflate`/`reset`, and `SYNCHRONIZED` refusals such as
`CleanerImpl$CleanableList.insert`. So no call out of compiled code becomes a
direct jump, and that remains a genuine missed optimization. Its price, from the
post-fix profile: `jit_invoke_virtual_mic` 1.04%, `try_jit_site_cached_native_dispatch`
1.65%, `jit_invoke_dispatch` 0.89%, `decode_dispatch_values_into` 0.80%,
`try_jit_compile_callee` 0.75% — **the whole funnel is ~6%**, against a JIT that
now beats the interpreter by 36%. Worth doing on its own merits; not worth
doing as this page's fix.

**`forward_jit_reference_args` still re-parses the callee descriptor on every
call** (`vm/src/jit/helpers.rs`). Measured at 0.78% post-fix. Deliberately NOT
memoized: the obvious key is `jit_site_key(vm_identity, info_ptr)`, and this
helper runs **before** `flush_raw_entry_dispatch_caches()` — a `JitInvokeInfo`
box is freed with its `CompiledMethod`, so its address can already belong to a
different call site. That is the exact hazard the same function's neighbouring
comment exists to warn about. A correct memo needs the call moved after the
revalidation, which is a separate change with its own argument to make; 0.78%
does not buy it here.

**Two levers that governed nothing are now honest** (both were reached for while
diagnosing this, and each would have answered an A/B with the baseline twice):

* `CRATONVM_NO_PRECISE_JIT_MAPS` — the codegen decides on
  `precise_jit_maps_enabled() || moving_young_enabled()` and moving-young is
  default-ON, so the flag alone changed nothing about safepoint emission, and
  did so silently. It now warns once and names `CRATONVM_NO_MOVING_YOUNG=1`.
* `CRATONVM_TIER_C2_THRESHOLD` — `request_c2_upgrade`, the C1→C2 supersede the
  worker runs after every successful C1 publish and the door nearly all C2
  compiles come through, consulted no invocation count at all (2e9 still gave
  `c2=97` against a baseline `c2=95`). Setting the knob now gates that door too.
  Deliberately not applied when the knob is unset: gating the supersede at the
  default 20 000 would change which methods reach C2 for every workload, and
  nothing measured supports that. The gate exists so the trade can be priced on
  the gauntlet, not so it can be flipped.

**Do not raise the default `CRATONVM_JIT_THRESHOLD`.** The page's sweep — 500 →
286.30 s, 500 000 → 125.39 s, `--nojit` 143.06 s — was real, but it was a
proxy: a higher threshold compiles fewer methods, and compiled methods were what
was leaking monitors. Re-measure it before treating it as evidence about
admission.

## The second workload, re-measured — and it was never this page's defect

The page gained a section on 2026-08-10 (`ee4ec2c1f`) reporting the same arm
ordering on `org.h2.test.db.TestTransaction.testMergeUsing` — default-threshold
JIT worst at 108 ms, `--nojit` 92 ms, near-nothing-compiled best at 87 ms
against HotSpot's 22 — and the correctness consequence that at H2's own
`TestAll.lockTimeout = 50` ms the losing connection's whole MERGE batch dies and
the test asserts `Expected: 100 actual: 50`.

Re-measured on the fixed binary with
`apps/h2database-suite-runner/probes/MergeLockBudgetProbe contend 50 4`, three
reps, losing-thread batch ms:

| arm | batch ms | verdict |
|---|---|---|
| HotSpot 25 | 4–27 | **4/4 OK** |
| CratonVM before, JIT | 141–282 | FAIL |
| CratonVM after, JIT | 111–280 | FAIL |
| CratonVM after, `--nojit` | 107–242 | FAIL |

**The fix does not move this workload, and it was not supposed to.** The
JIT-vs-`--nojit` gap here is ~10–15%, not the 52–83% this page was about, and it
is the same before and after — because the leak's cost is quadratic in *distinct
objects locked from compiled code*, and this probe runs for about a second over
a handful of monitors. Nothing accumulates.

What remains is a plain throughput gap of ~5–10x HotSpot, and it already has an
owner that characterises it correctly and independently:
`performance/h2-update-path-throughput-RETIRED-20260821.md`, which states in as
many words that **`testMergeUsing`'s 50 merges never warm up at all, which is why
its failure is identical with and without the JIT**. That sentence and this
measurement agree, and both say the class is not evidence about tier-up
admission. The second-workload section is therefore recorded here as re-measured
and reattributed, not carried forward as an open residual of this page.

## Gates

`regression-suite/bridge-ratchet.sh` on the fixed binary: **BRIDGE-RATCHET PASS**
(9236 / 4557, both below the frozen 9528 / 4581). The two new `PrintStream`
registrations appear as added rows with `kind=bridge`, matching their two
existing siblings' kind and image adjudication exactly; the kind-map gate treats
additions as out of scope by design.

The kind-map gate DOES fire, on `java/lang/Integer.toString(II)` and
`java/util/Iterator.remove()V` (`bridge -> intrinsic`), and neither baseline was
re-frozen here. A control run of the same gate against the **pre-change** binary
produces the identical two lines and the identical 389 removed rows, so both the
firing and the ratchet's "IMPROVED, lock it in" are pre-existing drift between
dev and the frozen baselines. Re-freezing them inside this change would launder
someone else's re-tag and claim credit for 294 removals it did not make.

## A second confirmed victim: `CachesEndpointWebIntegrationTests`

`module/spring-boot-cache`'s `CachesEndpointWebIntegrationTests` was showing as
a deterministic HANG at 1500s (default GC, `dev@4c4fb3902`, the commit just
before this fix landed) in an otherwise-clean rerun of the 22-class
FAIL/HANG union from the 2026-08-08 non-passed reruns — the one class in that
batch that did NOT resolve at 5x the original 300s budget, and looked like a
real stuck-forever bug rather than timeout-boundary noise.

Its `.out.log` showed steady, non-stalled progress the whole time (repeated
Tomcat/Jersey/WebMvc/Netty context start-stop cycles — `@WebEndpointTest` runs
each of the class's 7 test methods against 3 web-server backends, 21 cycles
total), but each `Root WebApplicationContext: initialization completed in N
ms` line grew dramatically across the run: 2722, 3598, 7179, 20384, 26887,
72903, 39913, 101569, 50401, 114912 ms — a >40x cost increase over ~10 of the
21 expected cycles, with a zero-line `.err.log` (no GC/JIT diagnostic
warnings at all, ruling out the unrelated `[moving-young]` fallback death
spiral documented elsewhere for other classes). That per-cycle-growing-cost,
GC-silent shape is exactly this bug: `CachesEndpointWebIntegrationTests`
exercises the actuator's `CachesEndpoint`/`CacheManager` machinery across many
synchronized Spring infrastructure objects, all within one long-running
process — precisely the `jmx_locked_monitors` growth condition this page
describes, just reached through many small `synchronized` calls across 21
context cycles rather than one call-dense method.

Rebuilt at `dev@92b35f1e5` (this fix included) and reran the single class
alone: **PASS, 74.6s total** (all 21 cycles), vs. not finishing at all in
1500s before. Confirms the fix is not narrow to `ZipContentTests` — any
long-running single-process class with enough cumulative `synchronized` use
was affected, and this "HANG" was never a real deadlock.
