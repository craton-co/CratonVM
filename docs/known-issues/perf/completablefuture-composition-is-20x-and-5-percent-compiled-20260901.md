# `CompletableFuture` composition is ~20x, and only 4.9 % of it is compiled code

## Status
**OPEN, opened 2026-09-01, four hypotheses tested and refuted 2026-09-02.**
The successor to
`performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`
(internal), which discharged all four of its own residuals and left this.

The predecessor spent three revisions looking for the component that dominates
composition. **There is not one**, and this page has now spent a day
establishing that the three most obvious next moves are not it either. That is
the content: a flat profile plus four things it is NOT.

Everything below is measured on the binary at `eb79f5904` unless stated;
nothing here is inherited prose.

## Severity
**MEDIUM-HIGH and broad, but not urgent.** `CompletableFuture` composition sits
under reactive Spring, hibernate-reactive and Vert.x. It is not a correctness
issue and it is not a cliff — the cost is flat and predictable. What makes it
worth a page is that the fix is structural, so it will not arrive by accident.

## The measurement

`HibfixComposeProbe2` — no scheduler, no locks, no cross-thread handoff, each
thread owning its futures end to end, `wrong=0` in every run. CPU time,
because the host of record has not been idle once (load 7-370 across this
session) and wall clock there measures the other tenants; see the
predecessor's "Why the idle box stopped being required".

| | CratonVM | HotSpot | ratio |
|---|---:|---:|---:|
| cpu us per chain, 2 threads x 640 000 chains | 34.6 | 0.609 | **~57x** |
| cpu us per chain, 2 threads x 160 000 chains | 32.4 | 1.938 | 17x |
| whole-process cpu, 2 threads x 160 000 chains | 5.02-5.34 s | 0.22-0.33 s | ~20x |

The spread across those rows is HotSpot's warmup, not CratonVM's variance —
see "What amortisation says" below.

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
it.** Every attempt on this workload has looked for a hot component, found a
5 %-shaped one, and measured a 5 %-shaped win — the CAS bind moved it 1.10x,
the reference-read bind 1.00x. That is not three failed hypotheses; it is one
structural fact seen three times.

## Four things it is NOT

Each of these was a reasonable next move, each is now closed with a number, and
none of them should be re-tried without new information.

### 1. It is not a missing IR-tier inline — and the flag barely engages

`CRATONVM_JIT_IR_INLINE=1` is the lever the predecessor page nominated first,
on the grounds that a chain of tiny methods that never amortises is a chain
that is not being inlined. One binary, one switch, six interleaved reps,
2 threads x 320 000 chains:

| | cpu | us/chain |
|---|---:|---:|
| `CRATONVM_JIT_IR_INLINE=1` | 21.14-23.79 s (median **22.71**) | 35.49 |
| default (off) | 20.74-24.61 s (median **22.96**) | 35.88 |

**1.011x, ranges overlap.** And the engagement count says why it cannot help:

| workload | bodies spliced |
|---|---:|
| netty, 200 classes (the 2026-08-28 soak) | **11 030** |
| hibernate `ASTParserLoadingTest` (same soak) | **355** |
| **composition, 640 000 chains** | **12** |

Twelve. The flag is not inert — the soak proved that — it simply has almost
nothing to splice here. Whatever is keeping composition uncompiled is upstream
of inlining.

### 2. It is not the tier-up nomination threshold

`CRATONVM_JIT_THRESHOLD` (default 500) is what gates a method being NOMINATED
to the tier manager at all: `on_method_invocation_observed` is called from
inside the `if n >= threshold`, and that call is what creates the entry. So the
`distinct methods tracked` number is a census of NOMINATED methods, not of
interpreted ones — which is the correction the predecessor page owes about
`hot_but_stuck_in_interpreter=0`. Lowering it:

| `CRATONVM_JIT_THRESHOLD` | cpu s | us/chain | tracked | still-interp | c1 | c2 | stuck |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 500 (default) | 22.84 | 35.70 | 8 | 1 | 4 | 2 | 0 |
| 100 | 22.67 | 35.43 | 10 | 4 | 2 | 3 | 0 |
| 20 | 22.63 | 35.36 | 14 | 8 | 4 | 2 | 0 |

A 25x lower threshold buys 6 more tracked methods and **no time at all**. The
extra nominees do not even get compiled — `still-interpreted` rises with
`tracked`. So the small tracked set is not the threshold hiding work; the
workload really does have very few distinct hot Java methods.

`CRATONVM_DBG_TIER_ENQUEUE=1` names them, and they are the right ones:
`CompletableFuture.postComplete`, `.encodeRelay`, `.reportJoin`,
`Objects.requireNonNull`, and the probe's own `chain` / `lambda$chain$0`. The
tier system is not blind here and it is not stuck.

### 3. It is not a synthetic stub standing in front of cheap bytecode

This was the most promising lead on the page and it is refuted in the opposite
direction, which is why it gets a switch of its own
(`CRATONVM_NATIVE_CF_COMPLETE`, default ON, **do not flip it**).

`java/util/concurrent/CompletableFuture.complete(Ljava/lang/Object;)Z` is a
registered `Bridge` native taking **200 000 crossings, 2.50 per chain**, and
its registration comment justifies it by `CompletableFuture.NIL` being
"effectively null on CratonVM". **That premise is stale** — read directly,
`NIL` is a proper `AltResult` with `ex = null`, identical to HotSpot, and with
the registration off the real bytecode answers `complete(null) -> true,
isDone=true, get=null` correctly.

That is the exact shape of `AtomicReference.compareAndSet`'s stub, which was
de-registered on 2026-08-29 for a **1.6x win**. So the same move here should
pay. Six interleaved reps, one binary, one switch:

| | cpu | us/chain |
|---|---:|---:|
| registered (default) | 20.64-23.55 s (median **22.16**) | 34.6 |
| de-registered | 66.56-70.49 s (median **69.52**) | 108.6 |

**3.14x SLOWER without it**, ranges disjoint, `wrong=0` in all twelve runs.

The native census gives the mechanism, and it is not "the bytecode is slow" —
the boundary crossing does not disappear, it **multiplies** (80 000 chains):

| native | stub ON | stub OFF | delta |
|---|---:|---:|---:|
| `CompletableFuture.complete` | 200 000 | 0 | −200 000 |
| `CompletableFuture.completeValue` | 0 | 200 000 | **+200 000** |
| `Unsafe.compareAndSetInt` | 619 | 200 000 | **+199 381** |
| `Object.<init>` | 2 882 | 122 174 | **+119 292** |

The real `complete` is `completeValue(value)` — itself a registered native, so
the boundary is crossed anyway — plus the CAS and the `AltResult`/`Completion`
allocation that the stub collapses. **It is a fast path in front of three more
crossings and an allocation, not a shadow in front of cheap bytecode.**

The general lesson is worth more than the row: on this VM a synthetic stub is
only a tax when what it shadows is *pure* bytecode. Where the shadowed method's
own callees are natives too, removing the outer stub buys nothing and pays for
the inner ones.

### 4. It is not locks, and it is not thread scaling

`locks / monitors` is 0.2 % of the profile, which retires the family of
contention hypotheses inherited from `HibfixComposeProbe` (whose profile was
~45 % scheduler — but that probe used a `ScheduledThreadPoolExecutor` and this
one deliberately does not). Thread scaling costs CratonVM **1.09x** across a
24x thread increase and HotSpot 1.85x; CratonVM is the flatter of the two.

## What amortisation says about where to look

Two sweeps, CPU us per chain, one variable at a time:

**Total chains fixed at 480 000, threads vary:** CratonVM 30.67 -> 33.52 across
1 -> 24 threads (**1.09x**). HotSpot 0.708 -> 1.312 (**1.85x worse**).

**2 threads, total chains vary 40 000 -> 640 000:** CratonVM 36.50 -> 31.66
(**1.15x**). HotSpot 5.000 -> 0.609 (**8.2x**).

**CratonVM does not amortise and HotSpot does.** CratonVM reaches steady state
almost immediately at ~31 us/chain; HotSpot keeps improving by 8.2x over the
same range. A chain of tiny JDK methods that costs the same on call one million
and call one is a workload that is not being inlined — and finding #1 says the
inliner is not the missing piece, because it has nothing to work on.

## Where to look next

The three cheap moves are spent. What is left is structural, and the ordering
below reflects what the four refutations imply rather than what looks hot.

1. **Why is there so little compiled code to inline?** The tier system
   nominates the right methods (finding #2) and the inliner splices 12 bodies
   (finding #1). Those two facts together are the open question: with
   `postComplete`, `encodeRelay` and `reportJoin` all compiled, what is the
   4.9 % actually made of, and which frames is `execute_frame_from_index`
   running? A `perf` profile with the JIT frames resolved, or a sampling of
   `execute_frame_from_index`'s `f.method_name()`, would answer it directly and
   nobody has taken it.
2. **`Integer.intValue` at 3.99 crossings per chain** is now the LARGEST native
   crossing on this workload — larger than the CAS, larger than `complete`. A
   thin direct bind for it exists (`DirectNativeShadow::IntegerIntValue`) and
   is plainly not engaging at these sites; the census is 319 018 crossings in
   80 000 chains. Establish why before anything else in this list: it is the
   only remaining item with a bind already written.
3. **Name-keyed lookup, 8.2 %.** Per-call string hashing on a dispatch path
   (`HashMap<&str,()>::contains_key` 1.24 %, `__memcmp_evex_movbe` 2.54 %,
   `NativeMethodRegistry::find_with_kind` 0.70 %). The question is which call
   sites reach `find_with_kind` and `invoke_on_class_shared_inner` with a NAME
   rather than a resolved id.

## What is already excluded, with the evidence

Do not re-derive these.

* the four findings above, each with a number;
* **`VarHandle` READ binds of any kind.** `VarHandle read thin direct calls:
  served=0 declined=0` on this workload — composition makes no compiled
  VarHandle read calls at all, so no read bind can reach it. This is why the
  2026-09-01 reference-read bind, which is 2.12-2.18x on the per-op probe, is
  1.00x here;
* **the `VarHandle` CAS funnel.** Bound; 1 677 766 served, 0 declined;
* **compile refusals of the 2026-08-27 kind.** Both fixed, and restoring either
  with its kill switch still costs 2.19x and 2.88x, so the instrument works;
* **Java-frame profiling.** It attributes a whole native call to the Java frame
  that made it, and it produced "34 % in `tryPushStack`" once already, which
  sent three days into `VarHandle` primitives that were not the constraint.
  Use `perf` on the binary.

## Repro

```bash
javac -d <out> apps/probes/HibfixComposeProbe2.java
/usr/bin/time -f "cpu user=%U sys=%S wall=%e" \
  cratonvm --java-home <jdk> -Dprobe.threads=2 -Dprobe.chains=320000 -cp <out> HibfixComposeProbe2
# the levers this page spent, each one binary and one switch:
CRATONVM_JIT_IR_INLINE=1        # finding 1
CRATONVM_JIT_THRESHOLD=20       # finding 2
CRATONVM_NATIVE_CF_COMPLETE=0   # finding 3 -- 3.14x SLOWER, do not flip the default
# engagement, which needs no quiet box:
cratonvm ... --dump-native-registry /tmp/reg.json
CRATONVM_DBG_IR_COMPILES=1 cratonvm ... 2>&1 | grep -c 'inline-plan'
CRATONVM_DBG_TIER_ENQUEUE=1 cratonvm ... 2>&1 | grep 'enqueue'
```

Read CPU time, not wall. Report `/proc/loadavg` beside every row. Use at least
320 000 chains: the amortisation sweep puts anything smaller on HotSpot's
warmup curve, and the IR-inline soak warns that short vectors never tier up at
all.

## Related

- `performance/juc-primitives-and-composition-after-the-compile-refusals-CLOSED-20260901.md`
  (internal) — the predecessor, and the source of every "already excluded" row.
- `performance/ir-inline-gauntlet-soak-20260828.md` (internal) — the soak whose
  netty and hibernate engagement numbers finding #1 is measured against.
- [`interpreted-invoke-cost-350ns-20260825.md`](interpreted-invoke-cost-350ns-20260825.md)
- [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
