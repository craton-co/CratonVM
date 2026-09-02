# `JpaLargeBlobTest.jpaBlobStream` — 232 s against a 120 s timeout, and all of it is the per-native-call floor

## Status

**OPEN, and the page's model needs replacing. Re-measured 2026-09-02: the
decomposition below is right in its PROPORTIONS and wrong in its conclusions.
The cause is not a "per-native-call floor" — it is that an operation which is
fast when the JIT emits it INLINE stays at interpreter cost when it sits inside
a CALLEE, however that callee is reached. Section 9 is the finding and section
9.1 corrects an earlier, wrong version of it. Read both before acting on
anything above.** This is the residual of
`fixed-suite-bugs/hibernate/jpalargeblob-random-state-side-table-FIXED-20260830.md`, which is retired: both of
that page's own findings are fixed, the test got 1.35x faster, and it still
fails. What is left is not a defect in `Random`, in blobs, or in H2 — it is this
VM's per-native-call cost, and it needs a JIT change nobody has scheduled.

Measured 2026-08-30 on CratonVM `dev@0739d6388`, JDK 25.0.3+9-LTS Temurin,
Windows, quiet host.

## The number

```
org.hibernate.orm.test.lob.JpaLargeBlobTest#jpaBlobStream
  ok=0 failed=1  test_ms=231841
```

232 s for a 100,000,000-byte stream = **2318 ns/byte**. `@Timeout(120)` is a
method-level annotation no runner property widens, so the budget is
**≤1200 ns/byte**. Real HotSpot runs the same test in ~7 s.

Down from **312 s** before the two fixes on the retired page (a leak, and a
CSPRNG syscall per `new Random()`).

## Where every nanosecond goes

`probes/BlobStreamCost.java`, same host, after both fixes. The fixture's
`read()` returns one byte per call and does: a virtual dispatch, a boxed-`Long`
compare, a boxed-`Long` decrement, a `new Random()`, and a `nextInt()`.

| component | ns/byte | isolated by |
|---|---:|---|
| the fixture's `read()` | **1468.5** | `stream boxed+new Random` |
| — BOXING | **812.2** | minus `stream prim +new Random` (656.3) |
| — `new Random()` + `nextInt` | **551.2** | minus `stream prim +shared Random` (105.1) |
| — loop + virtual dispatch | 105.1 | `stream prim +shared Random` |
| H2's blob write | ~850 | 2318 − 1468.5 |

HotSpot's whole `read()`: **34.0 ns/byte**.

**Both large components are the same thing.** This VM's floor for a native call
is roughly 300 ns, and the fixture makes five per byte:

* **boxing, 812 ns ≈ 3 calls** — `count > 0` is `Long.longValue`; `count--` is
  `Long.longValue` + `Long.valueOf`;
* **Random, 551 ns ≈ 2 calls** — `Random.<init>` and `Random.nextInt`.

## What would actually clear it

| change | resulting ns/byte | passes? |
|---|---:|---|
| nothing (today) | 2318 | no |
| intrinsify boxing only | ~1506 | no |
| intrinsify boxing **and** the `Random` calls | **~955** | **yes** |

So it needs both, and it is a JIT-intrinsics project rather than a fix to any
one library method.

> **The ROW is right and its `Random` half names the wrong method.** "It needs
> both" survives re-measurement; "the `Random` calls" is 87% the CONSTRUCTOR and
> 13% `nextInt`. See below.

---

# 2026-09-02 re-measurement

Everything above was reproduced, and three of its conclusions do not survive.
Measured on `dev@23e262ab1` with `probes/BlobStreamCostCpu.java` - the arms of
`BlobStreamCost` timed on `ThreadMXBean.getCurrentThreadCpuTime()` instead of
`System.nanoTime()`, each auto-scaled to span at least 40 scheduler ticks.

**Why a new probe.** This box is shared. Two runs of ONE binary, minutes apart,
gave `boxed Long counter` 218.7 then 346.1 ns/op (+58%) and `stream boxed+new
Random` 2439.5 then 4172.4 (+71%), with `Get-Counter` showing six spinning
shells, another session's `cratonvm` and a `rustc` co-resident. A five-way
decomposition cannot be built out of numbers with that spread. Every figure
below spans 45-179 ticks at `wall/cpu` between 0.99 and 1.09.

## 1. dev has NOT regressed, and this page's absolutes are host-state

The numbers below are ~3x this page's. That is not drift on dev: `0739d6388` -
**this page's own commit** - was rebuilt and run INTERLEAVED with the tip in one
load window, two rounds, and the two are indistinguishable (`stream boxed+new
Random` 4531 / 4609 / 4531 / 5000 ns/byte).  The page's absolutes belong to a
host state that no longer exists.

What DID survive is the shape, and it survived well:

| component | this page | 2026-09-02 |
|---|---:|---:|
| BOXING | 55% | 52% |
| Random | 38% | 42% |
| loop + dispatch | 7% | 6% |

So the decomposition and its ranking are sound. Its ns/byte, its "232 s against
a 120 s timeout", and the `~955` arithmetic above are host-dependent and should
be re-derived before being quoted.

## 2. The `Random` half is the CONSTRUCTOR, not `nextInt`

This page bills `new Random()` and `nextInt` together as "≈ 2 calls". Split -
an arm the original probe does not have - they are nothing like each other:

| arm | ns/op |
|---|---:|
| `new Random(seed).nextInt` | 1777.3 |
| `shared Random.nextInt` (the call alone) | 273.4 |
| **=> `Random.<init>(seed)`** | **1503.9** |

**87% constructor, 13% call.** Intrinsifying `Random.nextInt` - item 3 of this
page's plan - addresses the 13%.

The constructor is expensive because `java/util/Random.<init>(J)V` is itself a
registered native (`kind=intrinsic` in `--dump-native-registry`) whose body
takes TWO global write locks (`SEED_TABLE`, `GAUSSIAN_TABLE` in
`native-builtins/src/securerandom.rs`) and computes an identity hash to key
them, on top of the native-call funnel.

**Retiring the Random shadow is still correctly rejected, and now for a measured
reason.** The retired page rejected it because "the JDK's own `Random` keys on
an `AtomicLong` whose `get`/`compareAndSet` are themselves natives here". `get`
is FAST (2.41 ns); `compareAndSet` is **361.3 ns, 27x HotSpot's 13.25**. Running
`Random.next(int)`'s exact loop on a real `AtomicLong` costs **2265.6 ns**
against the side table's 273.4 - 8x worse. The premise holds; it named the wrong
method.

> **CORRECTION, same day.** This section first concluded that
> `AtomicLong.compareAndSet` was the blocker, on the strength of that 2265.6 ns.
> `compareAndSet` WAS 27x and is now fixed (section 8) — and fixing it did not
> move `lcgNext` at all: 1945 -> 1949 ns across three pairs. The 2265.6 was
> never the CAS. See section 9.

## 3. There is no "per-native-call floor"

This page's central model - "roughly 300 ns for a native call, five per byte" -
does not hold. Within ONE class the cost spans 700x:

| `AtomicLong` method | ns/op | in an intrinsic region? |
|---|---:|---|
| `get()` | 2.41 | yes |
| `getAndIncrement()` | 12.02 | yes |
| `compareAndSet()` | 361.3 | **no** |
| `<init>(J)` | 1640.6 | **no** |

The free ones are exactly the ones `jit/src/lib.rs`'s `ATOMIC_LONG` region
already covers. A native call is not a floor; it is a call whose cost depends
entirely on whether the JIT has an inline emission for that method. That makes
this a per-method question, not a floor to be lowered.

## 4. What landed, and what it bought

`Long.longValue()J` / `Integer.intValue()I` now emit inline (`BOX_UNBOX` region;
`CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC` reverts). These are two of the three
boxing calls per iteration. **Both already had thin direct binds, engaged** - a
bind still emits a CALL with `needs_context: true`, and only inline emission
removes a call.

| | OFF | ON |
|---|---:|---:|
| `boxed Long counter` | 389.2 | **244.0** (1.60x, 3/3 pairs) |
| `boxed Integer counter` | 394.6 | **240.3** (1.64x) |
| `stream prim no Random` (zero-boxing control) | 45.6 | 45.0 (flat) |

Re-measured after merging 56 commits of `dev`, on the merged tip, arms in
OFF-then-ON order; the pre-merge figures were 305 -> 215 on the same lever.

Against HotSpot's 3.46 ns that is still 62x, and the remainder is now almost
entirely **`Long.valueOf`** - the harder half, needing an allocation fast path
plus the mandated `-128..127` identity cache (`Long.valueOf(127) ==
Long.valueOf(127)` must hold).

## 5. What is left, in size order

1. **`Long.valueOf` / `Integer.valueOf` inline** - the whole remaining boxing
   term, ~52% of `read()`.
2. **`Random.<init>`** - ~37% of `read()`. Either make the native cheap (drop
   the identity hash and the two global locks; the seed can live in the
   object's own field) or retire the shadow, which first needs (3).
3. ~~**`AtomicLong.compareAndSet` -> `LOCK CMPXCHG`**~~ **DONE, section 8.**
   34.7x, now 1.23x HotSpot. It was NOT the precondition for retiring the
   Random shadow — section 9 has the one that is.
4. **`Atomic*.<init>` synthetic stubs** - `AtomicLong` / `AtomicInteger` /
   `AtomicReference` constructors are `SyntheticStub` natives shadowing a
   one-`putfield` JDK constructor, costing **5.2x** (1640.6 -> 316.4 under
   `--jdk-only`, landing exactly on an app-classpath clone's 327.2 and on
   `java.util.Date`'s 324.2). `probes/AtomicCorrectness.java` passes identically
   on HotSpot, stubs-on and stubs-off, including 4-thread contended CAS loops.
   **Not on this test's critical path** - `Random.<init>` is intercepted and
   never allocates an `AtomicLong` - but it is a VM-wide win.

Note (4) and (3) point OPPOSITE ways for the same family: the `<init>` stub is
5.2x WORSE than the JDK bytecode, while the `compareAndSet` stub is 4.6x BETTER
than it (361 vs 1667). A blanket "retire the atomic stubs" would be half right
and half a regression.

## 6. Two traps this re-measurement fell into

Recorded because both produced confident, wrong prose before a control ran.

* **`new AtomicLong(i)` costs 1719 ns and `new Random(seed)` costs 1771** - a
  causal chain was written out of that near-identity. `--jdk-only` broke it:
  `AtomicLong` moved 5.2x and `Random` did not move at all, because
  `Random.<init>` is intercepted and never allocates one. Coincidence.
* **The stream arm looked ~9% slower with the boxing intrinsic ON**, consistent
  in sign across 8 pairs. A zero-boxing control arm (`stream prim no Random`)
  shows the SAME signal, and the compile counters are identical ON vs OFF
  (`admitted=11 osr=9 osr_entered=54 deopts=0`). It is drift. An arm the change
  CANNOT affect is the only thing that could have said so.

## 7. Probes added

* `probes/BlobStreamCostCpu.java` - the CPU-clock, tick-scaled rewrite, plus the
  constructor/call split and the Random-free control arms.
* `probes/RandomCtorSplit.java`, `probes/AtomicCtorSplit.java`,
  `probes/CtorClassSplit.java`, `probes/AtomicCasCost.java` - the splits behind
  sections 2 and 3.
* `probes/AtomicCorrectness.java`, `probes/BoxCorrectness.java` - the semantics
  either change has to preserve, each green on HotSpot and on both settings of
  its switch.
* `probes/CpuClockCheck.java` - what the CPU clock's quantum actually is on this
  host (15.625 ms), measured rather than assumed.

`jit/src/lib.rs::try_resolve_intrinsic` already has the right shape — a
per-family match returning a `JitIntrinsic` that the x64 ladder emits inline,
as `Math.sqrt` and `Math.min/max` already do — and carries reserved empty
regions for exactly this kind of addition.

* `Long.longValue()J` is a field load behind a null check. Small.
* `Long.valueOf(J)` needs an allocation fast path plus the JDK's mandated
  −128..127 cache. Bigger, and the correctness surface is the cache identity
  (`Long.valueOf(127) == Long.valueOf(127)` must hold).
* `Random.nextInt` would need the generator state reachable from compiled code;
  note that **retiring the `Random` native shadow does NOT help** — it was built
  and measured at 2.6x–7.7x slower, because the JDK's own `Random` keys on an
  `AtomicLong` whose `get`/`compareAndSet` are themselves natives here. See the
  retired page.

**This is worth far more than this test.** A boxed-`Long` counter costs 140
ns/op here against HotSpot's 3.6 — that is every `Long`, `Integer` and
`Character` in every collection and every autoboxed loop in the VM.

## Repro

```bash
cd apps/hib-suite-runner
cratonvm --java-home <jdk25> --Xmx 3000m @common.args \
  MethodRunner org.hibernate.orm.test.lob.JpaLargeBlobTest jpaBlobStream

# the decomposition, no database needed
javac -d . probes/BlobStreamCost.java
cratonvm --java-home <jdk25> -Diters=300000 -cp . BlobStreamCost
java -Diters=2000000 -cp . BlobStreamCost      # HotSpot, for the control column
```

## Related

* `fixed-suite-bugs/hibernate/jpalargeblob-random-state-side-table-FIXED-20260830.md`
  — the page this came out of: the native-memory leak, the entropy-draw spec
  divergence, the Random-shadow retirement that was built and left off, and the
  measurement mistakes made along the way.

---

# 8. `compareAndSet` is fixed: 34.7x

`AtomicLong` / `AtomicInteger` `compareAndSet` and `weakCompareAndSet` now emit
one `LOCK CMPXCHG`. One binary, `CRATONVM_JIT_NO_ATOMIC_LONG_INTRINSIC` as the
lever, 3/3 pairs:

| | OFF | ON | HotSpot |
|---|---:|---:|---:|
| `AtomicLong.compareAndSet` | 565.6 | **16.3** | 13.25 |

27x behind HotSpot before, **1.23x** after. The only thing separating it from
its `get` (2.41 ns) and `getAndIncrement` (12.02 ns) siblings was membership of
the intrinsic region.

# 9. The real finding: an operation is fast INLINE and 35x slower inside a callee

Fixing `compareAndSet` did not move `lcgNext` — the method whose body is one
`get` plus one `compareAndSet` — by one nanosecond. That is what exposed this.

All arms below are one per process (`probes/TierOneArm.java`, `-Darm=`), 3 000 000
iterations, CPU clock, so the method-stats line describes that arm and nothing
else.

| arm | CratonVM | HotSpot |
|---|---:|---:|
| `AL.get()` in the caller's own loop | 52.1 | — |
| static call, arithmetic body | 83.3 | 0.42 |
| static call, callee body has a REAL loop | 62.5 | 0.44 |
| get + CAS **inlined into the caller** | 72.9 | 20.8 |
| the identical body in a **static** callee | **2546.9** | 20.8 |
| the identical body in a **virtual** callee | **2474.0** | 20.8 |
| the same plus a `do/while` (`= Random.next`) | 2572.9 | 15.6 |

**35x, and the elimination is complete:**

* not the loop — a callee with a real loop and no atomics is 62.5 ns;
* not the atomics — `AL.get()` in the caller's loop is 52.1 ns;
* not the `do/while` — removing it changes nothing (2500.0 vs 2572.9);
* not static-vs-virtual — a virtual callee is just as slow (2474.0);
* not tiering thresholds — `CRATONVM_TIER_C1_THRESHOLD=1` and
  `CRATONVM_TIER_OSR_THRESHOLD=1` change it by under 1%;
* not compilation — the boxed and primitive arms report **identical** compile
  counts (`c1=1 c2=3 osr=2 deopts=0`, `admitted=5`).

What is left is: **the work is fast when the JIT emits it into the method being
compiled, and pays interpreter prices when it sits one call deeper.** HotSpot has
no such cliff — 20.8 ns whether inline, static or virtual.

This governs this page's own workload. Two streams differing ONLY in the
counter's type, both dispatching a virtual `read()` per byte, in isolated
processes: **93.75 ns/op primitive against 2880.21 boxed**, with identical
compile counts. The boxing inside `read()` is paying the same cliff, which is
why section 4's `Long.longValue` intrinsic bought 1.6x on a tight counter loop
and nothing at all on the stream arm.

## What to ask next

Two candidates survive, and neither is confirmed:

1. **The class-guarded intrinsics do not take effect inside a callee.** Every
   one of them (`Atomic*`, `String`, and section 4's `BOX_UNBOX`) needs a
   resolved receiver class id, and declines when it is 0 — `AtomicLongFieldLayout::new`
   returns `None` for id 0 by construction. A compile door that classifies
   invokes without supplying `cp_invoke_class_id_resolver` would therefore
   disable every one of them silently, with no counter moving. The tree already
   records that `try_compile` is not the only door and that the others
   "classify invokes themselves".
2. **The callee is compiled but not ENTERED as compiled**, so its body runs
   interpreted and its natives cost interpreter prices (~1150 ns each; two of
   them is 2300, and the measurement is 2500).

The cheap discriminator between them is a per-site engagement counter read from
INSIDE a callee compile, which this pass did not build.

## 9.1 CORRECTION to the first version of this section

The first version of section 9, committed earlier the same day, said "the method
containing the boxing is not COMPILED". **That is wrong and the evidence is one
command away:** isolated per-arm runs report identical compile counts for the
boxed and primitive streams (`c1=1 c2=3 osr=2 deopts=0`). Both are compiled.

It was inferred from a `--nojit` comparison — the boxed stream gains 1.5x from
the JIT where the primitive twin gains 40.8x — which is a real observation with
a different cause: the boxed arm's cost is dominated by work the JIT does not
remove, so the RATIO is small without the method being uncompiled. A ratio is
not a tier.

# 10. Probes added by sections 8 and 9

* `probes/AtomicCasCost.java` — the CAS arms, plus the inline-vs-behind-a-call
  pair that isolated section 9.
* `probes/CallFloor.java` — what an ordinary Java call costs (static 8.67 ns,
  loop-bodied static 15.67, virtual 20.14, against HotSpot's 0.42 for all
  three), so "it is the call" can be ruled out rather than assumed.
* `probes/TierOneArm.java` — ONE arm per process (`-Darm=`), so
  `CRATONVM_DBG=jit-method-stats` describes that arm alone. This is what showed
  the boxed and primitive streams have identical compile counts, and it is the
  harness section 9's elimination table is built from.
