# One user-defined class definition permanently slows the whole VM ~1.8x

| | |
|---|---|
| **Status** | root-caused and FIXED 2026-08-03. **It does not close the Tomcat classes** — see [§ What this does NOT fix](#what-this-does-not-fix), which is measured, not assumed |
| **Severity** | medium — a real VM-wide pathology worth 3.5x on a class-parsing probe, but NOT the binding constraint on the deploy classes |
| **HotSpot** | unaffected — completely flat across the same provocations |
| **Discovered** | while re-deriving why `docs/internal/tomcat/{04,29,30}` are CLOSED and the tests still hang |

## The 8 hanging classes, classified

From `apps/tomcat/.suite/results/rerun3-20260803-4shard` (identical set on the
07-31 and 08-02 runs):

| class | what it is |
|---|---|
| `catalina.startup.TestHostConfigAutomaticDeploymentAddition` | embedded deploy |
| `catalina.startup.TestHostConfigAutomaticDeploymentDeleteB` | embedded deploy |
| `catalina.startup.TestHostConfigAutomaticDeploymentDeleteC` | embedded deploy |
| `catalina.startup.TestHostConfigAutomaticDeploymentModification` | embedded deploy |
| `catalina.startup.TestHostConfigAutomaticDeploymentUpdateWarOffline` | embedded deploy |
| `naming.TestEnvEntry` | embedded start/stop, once per `@Test` |
| `coyote.http2.TestHttp2Section_8_2` | 6 658 parameterised cases at ~22x HotSpot; 2 043 done in 1 500 s. Not a hang — it needs ~4 900 s |
| `tomcat.util.http.TestMethodPerformance` | the ~400x charset chain; `docs/internal/tomcat/30-...-CLOSED.md` owns it and is right that it is re-homed |

So **six of the eight are one family**, and none of the three CLOSED documents
is wrong about the item it closed — they are closed on the *causes they name*.
What none of them measured is that this family **degrades within a single
process**, which is what turns "slow" into "cannot finish".

The evidence was already in the suite logs and had not been read as a series.
`TestEnvEntry` starts and stops an embedded Tomcat once per `@Test` over the
same webapp — identical work every time:

| start-up | 1 | 2 | 3 | 4 | 5 |
|---|---|---|---|---|---|
| seconds | 118 | 201 | 255 | 275 | 318 |

`TestHostConfigAutomaticDeploymentAddition`'s own `HostConfig` "has finished in
[N] ms" lines say the same: **104 s, 162 s, 260 s, 298 s**. A flat 100x
interpreter tax cannot produce a rising series.

## Reproduction: 20 seconds, no Tomcat startup

`probes/DeployRepeatCostProbe.java` alternates two halves of a deploy — BCEL-
parsing every `.class` in the webapp's JARs (Tomcat's annotation scan) and
loading every class through a fresh `URLClassLoader` (what a redeploy does) —
and prints **every** round instead of the minimum, which is the statistic that
hid this:

| | round 1 | 2 | 3 | 4 | 5 | 6 |
|---|---|---|---|---|---|---|
| CratonVM, parse only (ms) | 462 | 475 | 427 | 446 | 488 | 468 |
| CratonVM, load only (ms) | 26 | 18 | 17 | 18 | 23 | 21 |
| **CratonVM, interleaved — parse (ms)** | **472** | **815** | **809** | **807** | **826** | **909** |
| HotSpot, interleaved — parse (ms) | 44 | 10 | 5 | 5 | 11 | 5 |

Each half is flat on its own. Run together, parsing is permanently ~1.8x
slower. **Identical with `--nojit`** (430 → 783 → 838 → 844 → 821 → 878), so
this is not JIT invalidation, not tier-up, and not an admission ban.

`probes/LoaderStepCostProbe.java` narrows it to a single event:

| after | parse ms |
|---|---|
| baseline 1 / 2 / 3 | 476 / 490 / 448 |
| + construct a `URLClassLoader`, load nothing | 466 |
| **+ load ONE class through it** | **830** |
| + load all 156 | 760 |
| + a second loader, all 156 again | 931 |
| + a third | 833 |

Constructing the loader is free. **Defining one class through it costs 1.8x,
forever, on work that has nothing to do with that loader.** HotSpot reads
5 ms on every one of those rows.

Neither `CRATONVM_LOADER=-unload` (GC loader pinning) nor
`CRATONVM_LOADER=-aware-resolution` moves it — both were checked before the
cause below was accepted.

## Root cause: a process-wide latch gating a per-class question

`native-builtins/src/classloader.rs` keeps `ANY_DEFINING_LOADER_REGISTERED`, an
`AtomicBool` that is set the first time any user-defined `ClassLoader` defines
any class. It exists as a fast path, and its own doc comment records what it
was worth: `defining_loader_for` was measured "at 13-37% of ALL executed
bytecode instructions in a Tomcat workload".

The latch answers **"does this process contain any user loader?"** where every
caller actually needs **"does THIS class have one?"** — and the two questions
diverge permanently the instant a webapp, an OSGi bundle, ByteBuddy, cglib or
Groovy defines its first class. From that moment the fast path is dead for the
whole run, including for the bootstrap and app-loader classes that are the
overwhelming majority of every lookup.

`new` is the amplifier. `opcodes.rs`'s `Instruction::New` resolves its
constant-pool entry **from scratch on every execution** — there is no per-call-
site class-resolution cache — and a constant-pool parse constructs one object
per entry. Each `new` of an app-loader class therefore ran, once the latch was
set:

* `defining_loader_for(referencing_class_id)` — the store's `std::sync::Mutex`
  plus a hash probe, to answer `None`; and
* `would_fabricate_synthetic_stub(name)` — a class-manager read lock plus a
  name lookup, feeding a `drive_defining_loader_load` that cannot do anything
  without a defining loader on the referencing class, so its only possible
  outcome was a no-op.

## Fix

Both halves keep exactly the predicate they were written for, evaluated per
class instead of per process.

1. **`defining_loader_for` gets a lock-free per-`ClassId` key-set mirror**
   (`defining_loader_bits`, 128 KiB allocated lazily on the first
   registration). A class the store has no entry for is answered without
   touching the `Mutex`, no matter how many other loaders exist. Maintained at
   the store's own three writers — `register_defining_loader`,
   `gc_reconcile_defining_loaders`'s retire pass, and
   `reset_loader_singletons`. A reader that sees a bit set and then finds the
   entry gone is an already-legal outcome; a reader that sees it clear loses
   the same race the latch already had against a concurrent first
   registration.
2. **The two `would_fabricate_synthetic_stub` pre-passes in
   `resolve_class_loader_aware` are gated on the referencing class having a
   defining loader**, which the function already computes, instead of on the
   latch. Strictly narrower and provably equivalent: the body is a
   `drive_defining_loader_load`, which returns `None` without one.

## Measured result

Interleaved A/B, one binary per arm, two rounds each, both orders:

| after | BASE r1 | BASE r2 | FIX r1 | FIX r2 |
|---|---|---|---|---|
| baseline 1 / 2 / 3 | 462 / 407 / 373 | 477 / 417 / 427 | 398 / 400 / 382 | 387 / 370 / 374 |
| + empty loader | 420 | 471 | 411 | 412 |
| **+ one class** | **685** | **764** | **395** | **453** |
| **+ all 156** | **1354** | **1414** | **412** | **385** |
| + second loader | 1674 | 1759 | 929 | 870 |
| + third loader | 1416 | 1345 | 938 | 862 |

The first-load step is **gone** — one loader now costs what no loader costs,
**3.5x** on the `+ all classes` row.

A **second, smaller step survives from the second loader onward** (~1.5x over
baseline, not ~3.5x). It appears exactly when the same class NAMES acquire a
second definition, which is what makes `ClassManager::classify_exact_name`
answer `Ambiguous` instead of `Unique` and pushes its callers onto slower
paths. That is a different mechanism, it is not fixed here, and a webapp
redeploy hits it every time because it re-defines the same names under a new
loader.

## What this does NOT fix

**It does not move the Tomcat classes, and that was measured rather than
assumed.** `org.apache.naming.TestEnvEntry` under a 400 s cap, four runs, both
orders — the score is how many `@Test` methods started:

| run | BASE | FIX |
|---|---|---|
| pair 1 | 4 (94, 101, 128 s per test) | 4 (86, 141, 160 s) |
| pair 2 (reversed order) | 3 (60, 181 s) | 5 (88, 89, 93, 120 s) |

Run-to-run variance on this class is larger than any effect, so **no
end-to-end improvement is claimed.** The reason is the flat term, which this
change does not touch: even at baseline the probe parses at 3 300–3 500 µs per
class against HotSpot's ~16 µs.

`--stack-dump-on-timeout=60` on `TestEnvEntry` puts the main thread in
`ContextConfig.processAnnotationsJar` → `java/io/BufferedInputStream.read([BII)I`
— the same frame
[`webapp-deploy-annotation-scan-interpreted-226x.md`](webapp-deploy-annotation-scan-interpreted-226x.md)
records for `TestManagerWebapp`. That doc's census is **stale on this tree**
and should be re-taken before anyone plans against it: it reports "eight
compiled methods, `grep -c '<init>'` of 0". Re-run 2026-08-03 over the same
scan, `CRATONVM_DBG=jit-compiled` now lists **21 methods including two
constructors** (`ConstantUtf8.<init>`, `ConstantClass.<init>`) — so the
"constructors never compile" framing no longer holds; they reach the JIT via
compiled callers such as the now-compiled `Constant.readConstant`. What is
still absent is precisely the hot frame: `BufferedInputStream.read`,
`DataInputStream.readUnsignedByte`, `ClassParser.parse`, `ConstantPool.<init>`.
Those are `synchronized` / `lock-try-finally` bodies, which is
`31-synchronized-code-never-jit-compiled-FIXED.md`'s subject and the VM-wide
native-call floor — a separate project, deliberately not attempted here.

A second untaken lever found on the way, recorded because it is cheap to
check and structurally real: `dispatch_virtual.rs`'s invocation-counter block
opens with `if !is_special`, so **`invokespecial` call sites never increment
the JIT invocation counter and never reach the tiered manager**. Every
constructor, private method and `super` call is therefore invisible to
tier-up, and can only be compiled as a side effect of being called from
already-compiled code. It is not the binding constraint on THIS workload (the
census above shows the ctors do get compiled by that side route), so it was
left alone rather than changed on speculation.

`TestHttp2Section_8_2` and `TestMethodPerformance` are unaffected by any of
this and stay where their own documents put them.

## Reproduction

```bash
javac -cp "$(cat apps/tomcat/.suite/cp.txt)" -d probes/out probes/LoaderStepCostProbe.java probes/DeployRepeatCostProbe.java
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g \
  -cp "probes/out;$(cat apps/tomcat/.suite/cp.txt)" \
  LoaderStepCostProbe apps/tomcat/output/build/webapps/examples/WEB-INF/lib
```

Read the ratio between the `baseline` rows and the `+one class` row. Absolute
milliseconds move with host load; the step does not.
