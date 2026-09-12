# The callback memo, the promotion question, and the door both of them were at the wrong end of

## Status
**CLOSED, 2026-09-11.** Opened 2026-09-02 as
`known-issues/perf/composition-native-callback-and-the-promotion-question-20260902.md`,
the successor to
[`completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`](completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md).
Both of its items are discharged, each with a mechanism, an instrument and a
number:

| item | disposition |
|---|---|
| #1 every native→Java callback resolves the callee by NAME | **BUILT, MEASURED — and the page was measuring the wrong population.** The resolved-handle memo is `1.92x` on a workload that exercises it and `1.00x` on composition, whose marginal rate at that door is ZERO per chain: its 1 415 callbacks are all boot, unchanged from 2 000 chains to 40 000. What its "what is left" table actually counts is **`Unsafe.compareAndSetInt` arriving from the compiled MIC's tail**, 2.49 per chain — a Java→native call, not a callback. That one is fixed too: `invokes(general)` **100 805 → 1 421**. See "#1" |
| #2 nomination and promotion are separable, so the 2026-08-05 "do not" is re-askable | **ANSWERED. The switch was at the wrong door, and so was the census.** `getNow`'s site is served by `execute_invokevirtual_fast_door`, which carried its own copy of the `java/util/` exclusion as a BITMAP and read neither 2026-09-02 switch. With both doors wired the same way, promotion takes `getNow` from **40 000 interpreted frames to 510** and total interpreted frames from **58 805 to 18 848** — and is worth 1.02x with overlapping ranges. The 2026-08-05 ~30 % regression **does not reproduce**. It still ships OFF, for a reason that is not the one the page gave. See "#2" |

Everything below is measured on this branch unless stated. The host is shared
(other builds ran during several of these windows) and every table says so.

## The instruments this needed, because three of them were lying

Three defects in the diagnostics came out first, and none of the numbers below
was readable until they were fixed. They are listed first deliberately: the
page could not answer its own item 2 because every instrument it reached for
gave a confident wrong answer.

1. **`CRATONVM_DBG_TIERUP_DECLINE`'s reason chain did not know about its own
   switches.** It still tested `receiver_is_java_util` and the exception table
   in the order and the sense they had *before* 2026-09-02 separated nomination
   from promotion, so with
   `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS=1 CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1`
   it reported `receiver_is_java_util` — a DECLINE — for sites the `&&` chain
   had in fact ADMITTED. It now distinguishes `admitted`,
   `nominated_promotion_barred_java_util`,
   `nominated_promotion_barred_exception_table` and the two pre-switch
   refusals.
2. **`execute_invokevirtual_fast_door`'s twelve decline strings were each one
   position late.** The `invoke_cache`-miss arm was a bare `return None` with no
   message, and every string below it had slid up one — so "receiver is an
   array" was reported for a polymorphic site, "callee is intercepted" for a
   non-zero intercept shape, "frame stack is full" for a disabled descriptor
   facts table, and so on down the function. All twelve are corrected and the
   missing one is added.
3. **`CRATONVM_DBG=dispatch-tally` could not report at the size its own page
   measures.** It dumped every 2²⁰ rows; `HibfixComposeProbe2` at 40 000 chains
   produces ~100 000. It now also dumps at exit, and it labels the DOOR that
   issued each `invoke_or_native` (`native_callback`, `jit_mic_tail`,
   `jit_bail_to_interpreter`, `jit_dispatch_virtual_tail`,
   `jit_dispatch_static_tail`) rather than keying only on the callee — without
   which item 1's central question, *how much of the general resolver's traffic
   is a native calling back into Java*, cannot be read off it at all.

Two new censuses join them: `CRATONVM_DBG_PROMOTE_REFUSE` (item 2) and
`CRATONVM_DBG_CALLBACK_MEMO` (item 1).

## #1 — the resolved-handle callback, and what the page's table was actually counting

### The mechanism

`vm/src/runtime/native_callee_memo.rs` is the general form of the
`postComplete` fix. Per `(VM, receiver class, method, descriptor)` it memoises
"this dispatch reached the ordinary bytecode tail, on this declaring class",
and thereafter replays `interpreter::execute` on that class — which is exactly
what `invoke_virtual_bytecode_only`, the hand-written form, does.

**It does not predict; it witnesses.** A predicate that had to agree with
`invoke_or_native`'s and `invoke_on_class_shared_inner`'s thirty-odd special
arms would be a second copy of the resolver, and `AGENTS.md` names that shape
directly ("do not add another hard-coded class-name allow-list. Several already
exist, in disagreeing copies"). Instead `arm` marks the dispatch,
`note_plain_bytecode_tail` fires at the ONE site that is the ordinary tail, and
a memo is filled only if the dispatch actually arrived there. Every special arm
returns before it, so every special arm is refused **by construction, including
ones added after this file was written**.

Invalidation is by epoch, one per thing that could make a filled memo describe
a dispatch that would no longer happen: `NativeMethodRegistry::generation()`,
`class_definition_epoch()`, `class_origin_epoch()`, the `SharedVm` address, and
a blanket refusal while `any_class_redefined()`. The table is thread-local —
the thing being removed IS lock traffic, and a shared table would put it back.

`CRATONVM_NATIVE_CALLBACK_MEMO=0` is the off-switch.

### It is 1.92x where it engages

`apps/probes/NativeCallbackMemoProbe.java` is new and exists because
`HibfixComposeProbe2` cannot price this: it drives `HashMap` and `TreeMap`
natives that must call a user `hashCode`/`equals`/`compareTo` — 7.8 callbacks
per operation. Engagement first, because a timing from a run whose memo never
engaged says nothing:

    [callback-memo] probes=2329020 hits=2327480 misses=1540 fills=94 evictions=68

99.93 % hit on 94 fills. Six interleaved reps, one binary, the probe's own
ns/op (hashmap + treemap medians), on the shipped build:

| arm | ns/op median | range | ratio |
|---|---:|---|---:|
| default | **15 863** | 15 328-17 720 | 1.000x |
| `CRATONVM_NATIVE_CALLBACK_MEMO=0` | **30 429** | 28 516-31 602 | **1.92x** |

Ranges disjoint by 10 800 ns. An earlier six-rep run on the build before the
capability change read 14 059 against 27 211 — **1.94x**, also disjoint — so
the figure does not depend on the other change on this branch. `sink` is
bit-identical in all twelve runs of both arms and both maps end at 64 entries,
so the arms compute the same answer.

### And it is worth nothing on composition, for a reason worth recording

Same census, `HibfixComposeProbe2`, 40 000 chains:

    [callback-memo] probes=1415 hits=1 misses=1414 fills=1 evictions=0

**1 415 at 2 000 chains and 1 415 at 40 000 — a marginal rate of ZERO
native→Java callbacks per chain, and exactly one memoisable triple.**
The `postComplete` fix already took the one that mattered. So the page's own
"what is left on the composition workload" table is not a table of callbacks,
and the door-labelled dispatch tally says what it is:

| door | count at 40 000 chains | marginal, per chain |
|---|---:|---:|
| `jit_mic_tail jdk/internal/misc/Unsafe.compareAndSetInt(Ljava/lang/Object;JII)Z` | **99 428** | 2.49 |
| `native_callback` (every row, summed) | ~1 400 | **0.00** — all of it is boot |

**98.6 % of `invokes(general)` is a compiled method calling a registered native
by name** — `jit_invoke_virtual_mic` hitting a receiver it has no compiled
entry for and falling through to `invoke_or_native`. The page's prediction that
the residue "HIT rather than miss" was right; its attribution to `complete` and
`completeValue`, and to the native→Java direction at all, was not.

### The capability refusal whose reason expired

The JIT already has the mechanism for this: `try_jit_site_cached_native_dispatch`,
a per-call-site native cache keyed on the registry generation. It served none
of the 99 428, and the refusal census said why in a number that could not be
read as a cost — *three refused SITES*, reason "capability-classified triple".

`resolve_native_site` refused every triple `capability::classify_native`
answers `Some` for, and its comment closed with: *"no leaf native is one — the
classified set is process spawn, library load, `Unsafe`, Panama, file and
socket I/O, none of which could satisfy the leaf contract in the first place."*
True when written. **836631dcc widened the cache from leaf natives to every
registered native, and from that commit the sentence stopped describing the
code**: `jdk/internal/misc/Unsafe` is `CapabilityKind::RawMemory`, so the
hottest native on any CAS-driven workload was excluded wholesale — the shape
the predecessor page named as "a workaround can outlive its defect, and its
comment is the last to know", here in the form where the workaround outlived
its own *scope*.

The gate is not skipped, it is **moved to the dispatch side**, which is where
the funnel runs it too (`invoke_or_native`'s "CAPABILITY GATE, dispatch site 1
of 3" is deliberately at the last point before the native runs, not at
resolution). The entry carries one `bool`, so an unclassified native — nearly
all of them — pays a not-taken branch, and a classified one pays exactly what
it pays through the funnel. A gate refusal declines the fast path rather than
raising: the funnel it falls through to raises the identical error from the
identical gate, so the fast path cannot invent a failure the slow path would
not have produced. `CRATONVM_JIT_SITE_CACHE_CAPABILITY=0` restores the refusal.

Read as the MARGINAL rate, which is what `lookup_census`'s own doc comment
says the ratio line cannot give you — three workload sizes, one binary each
way:

| chains | 2 000 | 40 000 | 80 000 | per chain |
|---|---:|---:|---:|---:|
| `invokes(general)`, before | 5 666 | 100 760 | 200 859 | **2.502** |
| `invokes(general)`, after | **1 421** | **1 421** | **1 421** | **0.000** |
| `find_with_kind`, before | 8 990 | 104 083 | 204 184 | **2.503** |
| `find_with_kind`, after | **4 745** | **4 746** | **4 746** | **0.000** |

Not "fewer": **none.** The general resolver's per-chain traffic on composition
is gone, and what is left is a fixed ~1 400-call boot cost that does not move
when the workload quadruples. `CRATONVM_DBG=intrinsic-stats` says where it
went — `compiled site-cached native dispatches (non-leaf): 0 → 99 421` at
40 000 chains — and total registry lookups fall 138 263 → 37 748.

### What all of that is worth: 1.05-1.08x, and the 2x2 that attributes it

Twelve interleaved reps, one binary, four arms, `HibfixComposeProbe2` 2 threads
x 160 000 chains, `wrong=0` in all 48 runs. **Another tenant was building
during reps 1-4** (rep 1's `capOFF` read 21 822 ms); medians over all twelve
and over the settled last eight are both given, because dropping reps is a
judgement and the reader should see it.

| arm | median (all 12) | median (reps 5-12) | range (reps 5-12) |
|---|---:|---:|---|
| default (cap fix on, memo on) | **5 309** | **4 853** | 4 412-5 818 |
| `CRATONVM_JIT_SITE_CACHE_CAPABILITY=0` | 5 564 | 5 250 | 4 752-6 451 |
| `CRATONVM_NATIVE_CALLBACK_MEMO=0` | 5 117 | 4 891 | 4 452-6 226 |
| both off | 5 589 | 5 241 | 4 699-6 006 |

Read it as a 2x2, which is why it was run as one: **the memo factor moves
nothing in either capability arm** (4 853 against 4 891; 5 250 against 5 241)
and **the capability factor moves ~1.08x in both memo arms**. That is the
attribution the noise cannot take away, and it agrees with the engagement
census — the memo has no per-chain opportunities here at all, so it *must*
read 1.00x, and it does.

Ranges overlap, so 1.05-1.08x is the honest figure and not a separation.
Removing 2.49 full `invoke_or_native` cascades per chain buys ~100 ns of a
~16 µs chain. **That is the page's own diagnosis confirmed from a new
direction: the cost is flat and structural, and the single largest identifiable
per-call item in it is 1.6 % of the chain.**

## #2 — the promotion question: the switch, and the census, were at a door the workload had left

### The observation the page could not explain

With both switches on it saw `getNow` "still 40 000 interpreted frames in
40 000 chains, unchanged from shipping", and could not choose between "the
promotion does not engage" and "it engages and this site declines for a reason
downstream of the prefix".

It was neither. Two facts, both counts:

* today's `CRATONVM_DBG_TIERUP_DECLINE` reports **629 rows in total and no
  `getNow` at all** — against the page's 46 364 with 39 998 for `getNow`;
* the new `CRATONVM_DBG_PROMOTE_REFUSE` reports **46 268 rows, 39 999 of them
  `getNow`** — the page's population, to within 0.2 %.

**The dispatches moved doors.** `execute_invokevirtual_fast_door` — the warm
monomorphic `VirtualBytecode` hit, which is nearly every hit — now serves them,
and `execute_invokevirtual_cached`'s tier-up chain, the only thing
`tierup-decline` instruments and the only thing the two switches reach, is not
on that path.

### The second copy of the exclusion

The fast door has its own tier-up gate, and its own `java/util/` test, spelled
as the `class_is_java_util` **bitmap** rather than as
`starts_with("java/util/")` — which is why a grep over the tier-up paths finds
only the other one. (The bitmap's own doc says it answers "the same question
(`receiver_is_java_util`)", so this was never in doubt once the two were put
side by side.) Neither 2026-09-02 switch reached it, and it still gated the
**counter** on the prefix — so the conflation the page's own change undid in
one door survived intact in the other. `getNow` was never counted, never
nominated, never compiled, and no census row named it.

Both doors now read the same two switches, and the default is bit-for-bit what
it was: with both off, `promotion_barred` is `handler_bearing || java_util` and
the door's admission test reduces to the
`exception_table.is_empty() && !has_native && !java_util` it replaces.

### With the switch at the right door

`CRATONVM_DBG_INTERP_FRAMES=1`, `HibfixComposeProbe2`, 40 000 chains, one
thread:

| arm | `getNow` interpreted frames | all interpreted frames |
|---|---:|---:|
| shipping default | 40 000 | 58 805 |
| `CRATONVM_JIT_VIRTUAL_NOMINATE_ALWAYS=1` | 40 000 | 58 141 |
| `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1` | **510** | **18 848** |
| both | 510 | 19 738 |

Three things to read off it. **Nomination alone still buys nothing** — the
page's finding, now confirmed at the door that actually serves the site: the
frame count does not move at all, which is the engagement form of its 0.994x. **Promotion is the lever**, and it is worth two thirds of every
interpreted frame on the workload, not just `getNow`'s. And **promotion alone
suffices**: the door's counter was gated on `promotion_barred`, so admitting
the promotion un-gates the counter with it, which is why the `NOMINATE_ALWAYS`
half is not needed there.

The refusal census follows the same shape. Default: 46 268 rows, 39 999
`door_receiver_is_java_util getNow`. Nominate: 46 555 rows, 39 999
`door_nominated_promotion_barred_java_util getNow`. Both: **7 077 rows total**,
and `getNow`'s largest remaining row is 499 `door_counter_below_threshold` —
the calls before the threshold, after which it is in compiled code.

### And it is worth 1.02x, with overlapping ranges

Eight interleaved reps, one binary, 2 threads x 160 000 chains:

| arm | median (ms) | range |
|---|---:|---|
| shipping default | 5 342 | 5 208-7 369 |
| `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1` | 5 221 | 4 941-6 756 |

1.02x favourable, ranges overlapping, 6 of 8 reps favouring the switch.
Deleting 39 500 interpreted frames per 40 000 chains — two thirds of every
interpreted frame the workload pushes — moves the workload by about two per
cent. Same lesson as #1, from the other end.

### The 2026-08-05 "do not" does not reproduce

`aqs-thread-handoff-latency-RETIRED-20260805.md` item 3 said admitting these
bodies costs ~30 %, and told anyone re-opening it to reproduce its two rows
first. That instruction is discharged, and the answer is that they cannot be
reproduced.

`probes/JavaUtilTierUpExclusionProbe.java` was deleted with `probes/` on
2026-08-29 and is rebuilt at
[`apps/probes/JavaUtilTierUpExclusionProbe.java`](../../../apps/probes/JavaUtilTierUpExclusionProbe.java),
with the arms INTERLEAVED and repeated and a median printed — the original's
single A-then-B pair is not a measurement at this spread, which is the
2026-09-02 page's own finding (±20 %) restated.

| | 2026-08-05 run 1 | run 2 | 2026-09-11, 6 reps | 10 reps |
|---|---:|---:|---:|---:|
| `ReentrantLock` (tier-up suppressed) | 30 653 ns/op | 28 754 | 1 346 | 1 408 |
| `MyLock extends it` (tier-up admitted) | 39 954 ns/op | 37 904 | 1 395 | 1 327 |
| **subclass speed** | **0.77x** | **0.76x** | **0.99x** [0.93-1.06] | **1.05x** [0.97-1.14] |

The absolute numbers moved 21x in five weeks, so the 0.77x was measured against
a VM that no longer exists. The two-receiver comparison also carries "different
class, different call site, different inlining" along with the tier-up, which
is what the switch was built to remove — so here is the same question as a
one-binary A/B on the BASE arm alone, six runs of six reps:

| arm | median (ns/op) |
|---|---:|
| shipping default | 1 199 |
| `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1` | 1 181 |

**1.02x favourable, ranges overlapping.** Admitting the `java/util/` promotion
on an uncontended lock loop costs nothing measurable.

### It still ships OFF — and the page's reason for the exclusion was wrong

The page called the prefix "a performance policy", against the exception table
which is "a correctness hazard". The exclusion's own origin says otherwise.
cb563d707 added it with:

> The generic-conversion regression reaches a hot java.util graph while Spring
> creates annotation and conversion metadata. Its instance-method tier-ups are
> independently JIT-safe at direct/static sites, but **this cached virtual
> route can publish a stale receiver-specific entry and then spin.**

A spin is not a slowdown. So the 30 % number was never the whole reason the
exclusion exists, and its failing to reproduce does not discharge the rest of
it. `CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL` therefore stays **default-OFF**,
and what would have to happen to flip it is now a specific, checkable thing
rather than a re-run of a stopwatch: **show that the stale receiver-specific
entry cb563d707 names can no longer be published** — the entry gate
(`RedefineGate`) and `jit_supersede_epoch` both post-date it — and gate the
flip on the Spring conversion-metadata vector that produced the spin. Until
then the arm exists, both doors honour it, and it is priced at 1.02x on two
workloads, which is a poor trade for an unretired hang.

## What is excluded, with the evidence

Everything the predecessor excluded is untouched and must not be re-derived:
IR-tier inlining (1.011x); the tier-up nomination THRESHOLD; de-registering
`CompletableFuture.complete` (3.14x SLOWER); locks and thread scaling (0.2 % of
the profile); `VarHandle` READ binds (`served=0` on this workload); shaving the
flag/native-lookup path (`CRATONVM_JIT_HOT_LOOKUP_CACHE`, 0.995x); Java-frame
profiling. Added here:

* **Making `find_with_kind` cheaper.** Still refuted, and #1 is the
  demonstration of the alternative: `invokes(general)` fell 71x by *removing*
  the lookup at 99 421 sites, and that is worth 1.05-1.08x. Shaving the hash
  was worth 0.995x.
* **The NATIVE half of the callback memo** — memoising "this callback
  dispatches to native id N" beside the bytecode half. Nominated and declined
  on its own engagement: on `NativeCallbackMemoProbe`, 99.93 % of callback
  probes are already served by the bytecode half, so the native half is
  bounded above by 0.07 % there — and it would have to replay the §7 dispatch
  routing and the capability gate to be correct. The bound, not an opinion, is
  why it is not built.
* **"Just count them" at the fast door.** Not re-tried:
  `interpreted-invoke-cost-350ns-20260825.md` § Untaken levers already built
  and reverted it (`CRATONVM_JIT=special-tierup` — counted invocations
  129 391 → 129 455, compiled census 672 → 674, wall-clock *worse*). #2 above
  is the other thing: not counting more sites, but letting the sites that are
  already counted promote.

## Repro

```bash
javac -d <out> apps/probes/HibfixComposeProbe2.java \
      apps/probes/NativeCallbackMemoProbe.java \
      apps/probes/JavaUtilTierUpExclusionProbe.java

# --- item 1 -----------------------------------------------------------------
# The memo, on a workload that exercises it. Engagement FIRST.
CRATONVM_DBG=callback-memo cratonvm ... -cp <out> NativeCallbackMemoProbe
cratonvm ... -Dprobe.reps=4 -Dprobe.rounds=100000 -cp <out> NativeCallbackMemoProbe
CRATONVM_NATIVE_CALLBACK_MEMO=0 cratonvm ... (same)      # 1.92x, disjoint

# The residue the page's table actually counts, and its door.
CRATONVM_DBG=dispatch-tally  cratonvm ... -cp <out> HibfixComposeProbe2
CRATONVM_DBG=native-lookups  cratonvm ... -cp <out> HibfixComposeProbe2
CRATONVM_DBG=intrinsic-stats cratonvm ... 2>&1 | grep site-cached
CRATONVM_JIT_SITE_CACHE_CAPABILITY=0 cratonvm ... (same) # 1.05-1.08x

# --- item 2 -----------------------------------------------------------------
CRATONVM_DBG=promote-refuse  cratonvm ... -cp <out> HibfixComposeProbe2
CRATONVM_DBG=interp-frames   cratonvm ... -cp <out> HibfixComposeProbe2
CRATONVM_JIT_VIRTUAL_PROMOTE_JAVA_UTIL=1 cratonvm ... (both of the above)
cratonvm ... -Dprobe.reps=6 -Dprobe.rounds=200000 -cp <out> JavaUtilTierUpExclusionProbe
```

Interleave the arms, six reps minimum, quote the ranges, and say when the host
was busy — this one was, for four reps of the four-arm table, and the table
says so. `CRATONVM_DBG=<token>[,<token>...]` and the bare `CRATONVM_<GROUP>_<NAME>`
spelling both work; the second prints a one-line notice naming the first.

## Gates

* Regression suite **93/93 scheduled vectors, 0 failures, 0 list/coverage
  errors, 0 harness-blindness flags** — twice, once before merging dev and
  again on the merged tree, which is the rule this line exists for (dev moved
  149 commits between the fork point and the merge; a 93/93 can become a 92/93
  on same-day dev). Every engagement number on this page was re-taken after the
  merge and holds: `invokes(general)` still 1 421 at both 2 000 and 40 000
  chains, `getNow` still 40 000 → 511 interpreted frames under the promotion
  switch, callback-memo hit rate still 99.9 %.
* `cargo test -p cratonvm-vm --lib` 2 666 passed / 1 failed, and the one is
  `ffm_group_layout_force_native_covers_member_layouts` — **confirmed
  pre-existing**: a `git worktree` on pristine `origin/dev` fails it
  identically, and nothing on this branch touches
  `force_native_over_real_jdk_bytecode` or the FFM layout lists (`git diff
  origin/dev...HEAD | grep -c` reads 0 for both). Before the dev merge the same
  suite was 2 666 / 0.
  `cargo test -p cratonvm-types -p cratonvm-native-api` clean — the first of
  those is what enforces the four-file flag contract (`flag_surface`,
  `flag_docs_generated`, `flag_declaration_guard`) for the four new switches.
* One new unit test, `the_slot_keyed_gate_answers_what_the_string_gate_answers`,
  because #1 makes a security gate reachable from a new place: it asserts the
  slot-keyed form against the string form in all three capability modes
  (`Permissive` allows and records once per kind per thread, `Audit` allows and
  tallies ungranted, `Enforce` refuses every time with the same
  `SecurityException` naming the same capability), plus the no-policy no-op.
* Four new declared switches, each one relaxed load or a `OnceLock` when
  unarmed: `CRATONVM_NATIVE_CALLBACK_MEMO` (default-ON),
  `CRATONVM_JIT_SITE_CACHE_CAPABILITY` (default-ON),
  `CRATONVM_DBG_CALLBACK_MEMO`, `CRATONVM_DBG_PROMOTE_REFUSE`.
* Every probe run on this page reports `wrong=0` / `CLEAN`, in both arms of
  every A/B, and `NativeCallbackMemoProbe`'s `sink` is bit-identical across
  arms.

## Related

- [`completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`](completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md)
  — the predecessor, and the source of every row this page inherited.
- [`juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`](juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md).
- [`../retired/aqs-thread-handoff-latency-RETIRED-20260805.md`](../retired/aqs-thread-handoff-latency-RETIRED-20260805.md)
  item 3 — the measurement item 2 re-opened, and did not reproduce.
- [`../../known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`](../../known-issues/perf/interpreted-invoke-cost-350ns-20260825.md)
  — its § Untaken levers names `execute_invokevirtual_cached`'s tier-up block
  as where the exclusions live; #2 above adds the second door.
