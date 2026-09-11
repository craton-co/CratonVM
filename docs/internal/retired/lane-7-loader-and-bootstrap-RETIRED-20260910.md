# Lane 7 — the class loader, the bootstrap, and the failure triage — RETIRED

**RETIRED 2026-09-10**, all four of its "done" conditions discharged. This is a
campaign record, not an operating page.

| condition (the page's own §7) | outcome |
|---|---|
| the `BuiltinClassLoader` link failure is fixed or priced | **FIXED** — two natives retired as a pair |
| the null-`Module` family is closed or classified | **CLOSED** before this lane opened, and re-measured |
| all 41 `AssertionError`s routed to an owning lane in a table | **ROUTED** — the table is in the durable record |
| the 20 loader rows retired or classified | **CLASSIFIED** — 19 of 20 fail precondition 4 |

**Where the durable facts went.** The defect, the mechanism, the fix, the
triage table and the 20-row classification are in the
`the-builtin-classloader-could-not-link-and-getname-was-never-tagged-20260910`
record under `docs/known-issues/jdk-only/`. The retirement itself, with its
measurement, is the doc comment on `RETIRED_SHADOW_L7_TRIPLES` in
`native-api/src/retired_shadow.rs`. The probes are
`apps/probes/L7ParallelCapableProbe.java`,
`apps/probes/L7LoaderBootstrapProbe.java`,
`apps/probes/L7UnnamedModuleSweep.java` and `apps/probes/ClassNameSweep.java`.
The traps this page collected are on the permanent operations page, not here.

Everything below is the page as it stood, annotated with what each section
turned out to be. Read it as a record of how the lane was scoped, not as
instructions.

---

## 1. Why this lane was the critical path — and two of its four rows were stale

The page opened with a taxonomy of the 108 failing corpus vectors under
`CRATONVM_ENFORCE_NATIVE_SHADOW=all`, and said of two families that they *"have
both since been closed (`VM.savedProps` published; `Class.getName` tagged)"*.

**One of those two was not true of `dev`.** `Class.getName` was still an ambient
`Bridge`, `apps/probes/ClassNameSweep.java` existed in no commit, and the
`ServiceConfigurationError` family the page retired on that basis was still
live — live enough that it killed the FIRST version of this lane's own sweep
probe, because `System.out.printf` reaches `LocaleProviderAdapter`.

That is the page's own §4 rule reading back on itself, and it is the single most
useful thing this lane produced: **a triage page is stale the day after it is
written, including about what it says is closed.**

## 2. Target 1: `BuiltinClassLoader` will not link — FIXED

Ten vectors, and the page was right that no field publish reaches it: it is not
a null field at all. `BuiltinClassLoader.<clinit>` throws
`InternalError("Unable to register as parallel capable")` because
`java.security.SecureClassLoader` was never entered into
`ParallelLoaders.loaderTypes` — a constant-`true` native answered for the
registration and a no-op native replaced the `<clinit>` that would have made it.

The page's instruction to **read the image before concluding a surface is
missing** was the right one and it paid: `javap -c` on
`BuiltinClassLoader.<clinit>` is what named the single `if`, and `getSuperclass`
is what showed the chain runs through `SecureClassLoader` rather than
`ClassLoader`.

What the page could not have predicted is that **its own §6 trap was the
mechanism**: the `<clinit>` runs from inside a native, so the shadow dial could
never have reached it, and only a registration-time refusal does.

## 3. Target 2: the null `java.lang.Module` — CLOSED, and already was

Closed on 2026-09-09 by lane 0's `Class.getModule` -> reviewed `Intrinsic`,
one day before this page was written. Re-measured, not assumed:
`apps/probes/L7UnnamedModuleSweep.java` is 10 of 10 identical across HotSpot,
`--jdk-only` unarmed and `--jdk-only` armed.

The page's caution about `Module.getLayer()` on the unnamed module stands and is
lane 0's; nothing was tagged here.

## 4. Target 3: the triage — DONE, and the framing needed one correction

The table is in the durable record. What the page got wrong is the sentence
above it: *"The 41 `AssertionError`s are individual semantic gaps in other
lanes' territory."*

**A large share of them are not in any lane's territory.** Routed by each
vector's own class javadoc rather than by its failing frame — an `AssertionError`
is thrown in the vector's own `check()`, so routing on the frame sends every
assertion to whoever owns `java/lang` — the armed failures split into three
kinds, and only the first is a lane's work:

```text
   23  VM-INTERNAL   the JIT, the collector, class unloading, the kind censuses
   18  L4            java/io, java/nio, sun/nio, jdk/internal/foreign
   14  L3            java/lang/reflect, java/lang/invoke, jdk/internal/reflect
   11  L5            java/util/concurrent, jdk/internal/misc, java/lang/Thread
    9  L2            java/lang remainder, java/math
    9  L6            java/net, java/security, javax/net, javax/crypto
    8  L1            java/util, java/text, sun/util, java/time
    7  L7            java/lang/ClassLoader, jdk/internal/loader  (this lane)
    6  FROZEN        lane 0 §2's unowned set: java/sql, java/awt, management
    2  L0            java/lang/Class, java/lang/Module
```

The second kind is the one the page's framing had no row for: a vector that
exercises the JIT, the collector or class unloading fails under the dial because
the dial changes dispatch VM-wide, not because a §1.4 shadow in some lane's
prefix answered wrongly. Handing `RJitTreeSubMapIter` to L1 because a `TreeMap`
appears in its assertion would hand L1 work it cannot do.

The third is lane 0 §2's own frozen/unowned set, which by construction has no
owner to route to.

The page's two rules were both load-bearing and both are now on the permanent
operations page rather than here:

- **a first-failure count cannot score a fix in a chain** — report
  "N first-failures removed, M new blockers named", never just the delta;
- **re-run the classification before citing it** — which is how §1 above was
  caught.

## 5. The 20 retirement rows — CLASSIFIED

Re-derived from a fresh dump and reconciling exactly with lane 0 §2: 20
bucket-A/B rows over 6 classes under `jdk/internal/loader/`, plus the 27
`java/lang/ClassLoader` rows lane 0 counts in its own 131.

**19 of the 20 are recorded in no vector of the 132-vector corpus**, so
precondition 4 — a dispatch observed by the instrument that produced the
improvement — is unmet for them. They are classified rather than retired, with
the instrument named. The one exception, `BootLoader.loadLibrary`, is dispatched
in 19 vectors and is the only row this corpus could price.

The page's "do them last" was right for the reason it gave, and the reason has
now changed: the hierarchy links, and what the rows lack is an instrument that
reaches them.

## 6. Traps — all four held, and one was the fix

Every one of this page's four traps was used in anger:

- **a refused `SyntheticStub` falls through to an older native, not to
  bytecode** — which is why `registerAsParallelCapable` had to be retired as a
  TRIPLE (three registrations, one of which owns the slot) rather than by
  editing the owning site;
- **the shadow dial cannot see a call that starts inside a native** — this was
  the mechanism of target 1, not merely a hazard while measuring it;
- **the regression suite cannot reach `jdk.internal.misc`** — a first draft of
  `L7ParallelCapableProbe` reflected on `ParallelLoaders.loaderTypes` and got
  `InaccessibleObjectException` on both VMs. The probe that shipped asks the
  same question through subclassing and needs no `--add-opens`;
- **`<clinit>` failures cascade** — `L7LoaderBootstrapProbe` wraps every row for
  that reason, and rows 1-2 passing while 3-5 failed is what separated "the
  registration bytecode is broken" from "the JDK loader classes are not
  registered".

## 7. Done

All four conditions are discharged, and the numbers that discharge them are in
the durable record rather than here. The one this page asked for by name, the
`all`-arm count with the breakdown that keeps it honest:

```text
  CRATONVM_ENFORCE_NATIVE_SHADOW=all, SUITE=all, TIMEOUT=180
      25 passed / 107 failed   ->   42 passed / 90 failed

      17 vectors flipped to PASS, none flipped the other way
      46 first failures removed (17 + 29), 29 new blockers named
      61 vectors fail at exactly the same place
```

and the unarmed control, which is the acceptance criterion rather than the
result: **132 passed / 0 failed, before and after.**
