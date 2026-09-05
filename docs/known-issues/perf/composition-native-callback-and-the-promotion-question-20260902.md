# A native that calls Java by name, and the promotion question that is now askable

## Status
**OPEN, opened 2026-09-02.** The two things left by
`completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
(internal), which discharged all three of its own residuals and measured
**1.20x** on composition. Neither item below is composition-specific, which is
why they are a page rather than a section there.

This page inherits that one's whole "already excluded" list — the four
refutations plus the three discharges — and **nothing on it should be
re-derived**.

## Severity
**MEDIUM.** Item 1 is a per-call tax on a shape this VM uses everywhere and has
a measured sibling: the identical fix at one call site was **1.154x** of a whole
workload. Item 2 is a question, not a defect, and its wrong answer costs ~30 %
on a lock loop — so it needs an arm, not an opinion.

## 1. Every native→Java callback resolves the callee by NAME

`NativeContext::invoke_virtual(receiver, "someMethod", "()V", &[])` is how a
registered native calls back into Java, and it is used by hundreds of stubs. It
routes through `vm_exec::invoke_or_native`, which asks the native registry for
`(class, method, descriptor)` — hashing the triple, probing the slot table, and
on a miss running the cold descriptor-quirk rewrite — before reaching
`invoke_on_class_shared_inner`, which then resolves the Java method by name
again.

**When the callee is ordinary bytecode, every one of those probes is a
guaranteed miss, on every call.** The closed page measured exactly one instance
of this: `CompletableFuture.complete`'s `postComplete()` callback produced

    miss  100 008  java/util/concurrent/CompletableFuture.postComplete()V

in 40 000 chains — 76 % of every missed registry lookup on the workload — and
routing that ONE call through `invoke_virtual_bytecode_only` was **1.154x of the
entire benchmark**, with ranges disjoint.

That fix is per-call-site and does not generalise: it works because a human
knows `postComplete` is bytecode. What is open is the general form.

**What is left on the composition workload after that fix**, per 40 000 chains:

| | count | per chain |
|---|---:|---:|
| `invokes(general)` (calls reaching `invoke_or_native`) | 100 818 | 2.52 |
| `find_with_kind` | 103 629 | 2.59 |
| `find` | 23 544 | 0.59 |
| descriptor-quirk rewrites | 19 325 | 0.48 |

So the general resolver still runs about one `find_with_kind` per call. These
remaining ones are natives (`complete`, `completeValue`), so they HIT rather
than miss — but the lookup is per-call either way, and `NativeCallSite` already
exists as a generation-guarded memo for precisely this question. It is reached
from the cached-invoke sites and not from this one.

**What would settle it.** A resolved-handle form of the callback — the native
memoises the callee once (native id, or "not a native, here is the Java method")
and the memo is invalidated by the registry generation the existing
`NativeCallSite` already keys on. Then measure it against the same probe.

**Do not** attack this by making `find_with_kind` cheaper: it is already a
prefiltered 128-bit digest probe, and the closed page's own refuted hypothesis
(`CRATONVM_JIT_HOT_LOOKUP_CACHE`, 0.995x) is what happens when you shave the
lookup instead of removing it.

## 2. Nomination and promotion are now separable, so the 2026-08-05 "do not" is re-askable

`execute_invokevirtual_cached` refuses tier-up for a receiver whose class name
starts with `java/util/`. That exclusion was added for a Spring collections
graph and catches all of `java.util.concurrent` as collateral;
`CRATONVM_DBG_TIERUP_DECLINE=1` reports 46 364 declines on the composition probe
and every meaningful row is that reason.

`aqs-thread-handoff-latency-RETIRED-20260805.md` item 3 measured
narrowing it and said **do not**: admitting a `ReentrantLock` loop cost ~30 %,
reproducibly, on `probes/JavaUtilTierUpExclusionProbe.java`.

**That measurement priced the conflated form.** The exclusion gated the
invocation COUNTER and the site PROMOTION together;
`CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS` (added 2026-09-02, **default-OFF**) now
separates them. What is measured so far:

| | tracked | ever invoked | C1 | C2 |
|---|---:|---:|---:|---:|
| default (nomination barred) | 7 | 5 | 3 | 3 |
| `=1` (nomination admitted, promotion still barred) | **19** | **17** | 6 | **12** |

and **0.994x** — nominating alone buys nothing, because `getNow` is then
compiled and still entered interpreted 40 000 times in 40 000 chains.

**The open half is promotion.** `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL`
(2026-09-02, default-OFF) is the arm for it: it relaxes ONLY the
`receiver_is_java_util` half of `promotion_barred`, leaving the exception-table
half alone, because that one is a correctness hazard (a handler-bearing callee
entered by a direct compiled call has no interpreter boundary at which its own
handler can be resumed) where the prefix is a performance policy.

It has been run, and it did not answer the question. What it produced:

* **the timing route is a dead end at this variance.**
  `probes/JavaUtilTierUpExclusionProbe.java` was deleted with `probes/` on
  2026-08-29 and is rebuilt here at `apps/probes/`. Four interleaved reps of
  three arms scatter 2 752-3 951 ns/op — ±20 % — and no arm separates from any
  other. The rebuild also does not show the 0.77x subclass effect (median
  `sub_over_base` 1.00), but it is a REBUILD and that is an absence in a
  different probe, not a refutation of the 2026-08-05 number;
* **the engagement route says promotion is not engaging.** A count is immune to
  load in a way a timing is not, and `getNow` is the whole question: with
  `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS=1 CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1`
  it is **still 40 000 interpreted frames in 40 000 chains**, unchanged from
  shipping. Either the promotion does not engage, or it engages and this site
  declines for a reason downstream of the prefix.

**Start there, not at the stopwatch.** `CRATONVM_DBG_TIERUP_DECLINE` reports the
first condition of the tier-up CHAIN; what is missing is the reason the
`jit_cache` probe or `execute_jit_call_decoded` then refuses, which is a second
census and the next thing to build. Reproduce the two
`JavaUtilTierUpExclusionProbe` rows before concluding anything about the
timings — that is the instruction its own page gave, and the ±20 % above is why
it still holds.

## What is excluded, with the evidence

From the closed page, and none of it should be re-derived:

* IR-tier inlining (**1.011x**; 12 bodies spliced on this workload against
  11 030 on netty);
* the tier-up nomination THRESHOLD (25x lower buys six tracked methods and no
  time — and the counter, not the threshold, is why);
* de-registering `CompletableFuture.complete` (**3.14x SLOWER** — the stub is a
  fast path in front of three more crossings and an allocation);
* locks and thread scaling (0.2 % of the profile; CratonVM is the flatter of the
  two VMs across a 24x thread increase);
* `VarHandle` READ binds of any kind (`served=0` on this workload);
* shaving the flag/native-lookup path (`CRATONVM_JIT_HOT_LOOKUP_CACHE`,
  **0.995x**);
* Java-frame profiling — it attributes a whole native call to the Java frame
  that made it. Use `perf` on the binary.

## Repro

The probe, the switches and the six engagement instruments are all listed in the
closed page's Repro block. Read the **user** CPU column, interleave the arms,
six reps minimum, and quote the ranges.

## Related

- `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
  (internal) — the predecessor, and the source of every row above.
- `juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`
  (internal).
- `aqs-thread-handoff-latency-RETIRED-20260805.md` (internal) — item 3,
  the measurement item 2 re-opens.
- [`interpreted-invoke-cost-350ns-20260825.md`](interpreted-invoke-cost-350ns-20260825.md)
