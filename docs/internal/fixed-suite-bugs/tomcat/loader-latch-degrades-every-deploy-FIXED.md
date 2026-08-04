# One user-defined class definition permanently slows the whole VM ~1.8x

| | |
|---|---|
| **Status** | **FIXED 2026-08-03, RETIRED 2026-08-04.** Both residuals this doc left open have been resolved — see [§ Residual 1](#residual-1-the-second-loader-step-does-not-exist) and [§ Residual 2](#residual-2-the-invokespecial-tier-up-lever) |
| **Severity** | medium — a real VM-wide pathology worth 1.6x on a class-parsing probe, but NOT the binding constraint on the deploy classes |
| **HotSpot** | unaffected — completely flat across the same provocations |
| **Discovered** | while re-deriving why `tomcat/{04,29,30}` are CLOSED and the tests still hang |
| **Still open elsewhere** | the deploy classes themselves — `known-issues/tomcat/webapp-deploy-annotation-scan-interpreted-226x.md` owns the flat term and stays OPEN |

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
| `tomcat.util.http.TestMethodPerformance` | the ~400x charset chain; `tomcat/30-...-CLOSED.md` owns it and is right that it is re-homed |

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

Reverting **either half alone leaves the pathology mostly gone** — half 1's
`Mutex` and half 2's class-manager read lock are each about half the cost, and
with half 1 reverted `defining_loader_for` answers `None`, which then switches
half 2 off too. A control build must revert **both**, or it silently measures a
nearly-fixed VM. That mistake was made once while producing the table below.

## Measurement

The original A/B used `probes/LoaderStepCostProbe.java`: one process, every
loader state measured in sequence with `System.nanoTime()`. On a shared host
that instrument cannot carry the conclusion — re-run six times on the 08-04
`dev` tip it reported the step in two runs, no step in three, and one run in
which the post-loader rows came out **twice as fast as baseline** because the
parse loop had JIT-compiled by the time they ran.

Two changes make it decisive:

* **`probes/LoaderStepOneShotProbe.java`** — one loader state per *process*.
* **User CPU time from outside** (`/usr/bin/time -f %U`), not wall clock, with
  `cpu(mode, N) - cpu(mode, 0)` so VM startup and the cost of defining the
  classes cancel and only N parse passes remain. CPU time measures work done
  rather than time waited; the table below reads the same at host load 41 and
  at load 110, where wall clock moved by 3x.

`probes/ab-oneshot.sh` alternates arms per measurement and flips the order each
repeat.

### Positive control

The instrument is only trustworthy if it can still see the defect, so the CTL
arm is a build with **both** halves of the fix reverted. Both arms built from
the same tree with identical flags (`CARGO_PROFILE_RELEASE_LTO=off`,
`CODEGEN_UNITS=16` — the shipped fat-LTO link does not fit in the host's free
memory; only the ratio is being read).

User CPU seconds for 8 parse passes, setup and startup subtracted:

| after | CTL (latch restored) | FIX (shipped) |
|---|---|---|
| baseline, no loader | 5.54 / 5.63 / 5.53 | 5.68 / 5.81 / 5.83 |
| + empty loader | 5.64 / 5.81 / 5.68 | 5.67 / 5.54 / 5.80 |
| **+ ONE class** | **8.84 / 9.16 / 9.18** | **5.57 / 5.70 / 5.33** |
| + all 156 | 9.25 / 9.69 / 8.15 | 5.99 / 5.83 / 5.49 |
| + second loader | 9.51 / 9.06 / 7.93 | 6.05 / 5.82 / 5.18 |
| + third loader | 9.17 / 8.98 / 8.96 | 5.49 / 4.41 / 5.66 |

Columns 1-2 are `--nojit`, column 3 is the default JIT. The control reproduces
the documented pathology — flat at *no loader* and *empty loader*, then
**1.60x / 1.63x / 1.66x the instant one class is defined**, and flat again
afterwards. The fix arm never steps at all.

## Residual 1: the second-loader step does not exist

The 2026-08-03 revision of this doc recorded that "a second, smaller step
survives from the second loader onward (~1.5x over baseline)" and attributed it
to `ClassManager::classify_exact_name` answering `Ambiguous` once the same
class NAMES acquire a second definition. **That is withdrawn.** Two independent
results:

**1. It is not in the data.** In the table above neither arm steps at
*+ second loader* or *+ third loader*. CTL sits at the same 8-9.5 s it reached
at *+ one class*; FIX stays at its 5.2-6.1 s baseline end to end. The original
~1.5x was the wall-clock instrument: a single process whose later rows are
measured after the JIT has warmed and after the heap has grown, on a host with
other tenants.

**2. The mechanism cannot reach the measured workload.**
`probes/LoaderAmbiguityStepProbe.java` gives each loader a **disjoint** jar, so
loader count and name ambiguity stop co-varying:

| after | 1 user loader | names defined twice? | parse ms |
|---|---|---|---|
| baseline | no | no | 636 / 615 / 630 |
| + loader A (impl jar) | yes | no | 573 / 557 / 557 |
| + loader B (spec jar) | 2 loaders | **still no** | 530 / 522 / 565 |
| + loader C (impl AGAIN) | 3 loaders | **yes** | 572 / 540 / 548 |
| + loader D (spec AGAIN) | 4 loaders | yes, all | 549 / 579 / 548 |

Ambiguity arrives at loader C and nothing happens. It could not have: the
measured workload BCEL-parses `org.apache.tomcat.util.bcel.*`, whose names are
defined exactly once by the application loader. The ambiguous names belong to
the taglib jars, which the parse loop never resolves — so
`classify_exact_name` never returns `Ambiguous` on this path at all.

`classify_exact_name`'s `Ambiguous` arm is still worth what its own doc comment
says for *correctness* (a caller that reads ambiguity as absence loads a second
copy — the Spring AOT `argument type mismatch` and Groovy `$_run_closureN`
families). It is simply not a throughput term here.

## Residual 2: the `invokespecial` tier-up lever

The 08-03 revision also recorded, without acting on it, that
`dispatch_virtual.rs`'s invocation-counter block opens with `if !is_special`,
so "every constructor, private method and `super` call is invisible to
tier-up". Checked, and **narrower than stated**:

* The gate is real. [`dispatch_virtual.rs:1956`](../../../../vm/src/runtime/interpreter/dispatch_virtual.rs)
  gates counting *and* promotion on `!is_special`, so a call site that has
  reached the inline cache stops counting for `invokespecial`.
* But it is not the only counter. The uncached route
  (`vm/src/runtime/interpreter.rs`, the `bg_compile` block) calls
  `increment_invocation` for every method regardless of opcode.
* And the consequence does not hold. `CRATONVM_DBG=jit-compiled` over
  `LoaderStepOneShotProbe lib all 6` lists **27 compiled methods, 3 of them
  constructors**; over `LoaderStepCostProbe` it lists **31, including
  `ConstantUtf8.<init>`, `Constant.<init>`, `ConstantClass.<init>`,
  `ConstantPool.<init>` and `JavaClass.<init>`**. Constructors compile.

So the lever is a real code gap with a refuted consequence, and it is **not a
loader-latch residual**. Closing it means letting more methods reach the tiered
manager, which is exactly the change that silently moves work off the
single-pass backend onto an IR tier that has no vectoriser — a documented
multi-x regression hazard that needs `regression-suite/perf/c2-reach.sh` and a
CratonBench pass to land safely. It is re-homed rather than carried here: see
`known-issues/tomcat/webapp-deploy-annotation-scan-interpreted-226x.md`
§ *Untaken levers*.

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
`known-issues/tomcat/webapp-deploy-annotation-scan-interpreted-226x.md`
records for `TestManagerWebapp`. Of the four frames that doc lists as "still
absent" from the compiled set, **two now compile** (`ClassParser.parse` in
every census taken 08-04; `ConstantPool.<init>` in the `LoaderStepCostProbe`
one). The two that never do are the JDK's own `synchronized` bodies —
`BufferedInputStream.read` and `DataInputStream.readUnsignedByte` — which is
`31-synchronized-code-never-jit-compiled-FIXED.md`'s subject and the VM-wide
native-call floor, a separate project deliberately not attempted here.

`TestHttp2Section_8_2` and `TestMethodPerformance` are unaffected by any of
this and stay where their own documents put them.

## Reproduce

Fixture: any directory of jars, plus Tomcat's BCEL on the classpath. The
2026-08-04 runs used the two `taglibs-standard-*` jars from
`apps/tomcat/output/build/webapps/examples/WEB-INF/lib` (156 classes) and a jar
of `org/apache/tomcat/util/bcel` from `apps/tomcat/output/classes`.

```bash
javac -cp "$(cat apps/tomcat/.suite/cp.txt)" -d probes/out \
  probes/LoaderStepOneShotProbe.java probes/LoaderAmbiguityStepProbe.java \
  probes/LoaderStepCostProbe.java probes/DeployRepeatCostProbe.java
```

The load-insensitive reading — the one to trust on a shared host:

```bash
bash probes/ab-oneshot.sh 2 8 --nojit
```

Read the ratio between the *baseline* rows and the *+one class* row. Absolute
CPU seconds move with the build profile; the step does not. A control build
must revert **both** halves of the fix (see § Fix) or it will read as flat.
