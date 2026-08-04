# Webapp deploy: BCEL annotation scan is 226x slower — it never compiles

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | high — blocks `TestManagerWebapp.testDeploy` + `.testBug57700`; makes every deploy-heavy Tomcat class 15–65x slower |
| **HotSpot** | PASS |
| **CratonVM** | FAIL (timing only — no wrong results, no crash) |
| **Discovered** | 2026-08-03, after fixing the `seek0`/`ExpandWar` defect that had been masking it (`fixed-suite-bugs/tomcat/testmanagerwebapp-expandwar-seek0-bad-fd-FIXED.md`) |

> **Update 2026-08-03 — two corrections, neither of which closes this doc.**
>
> 1. **The compiled-method census below is stale.** Re-run on merged `dev`
>    `5e1f7d6e3` over the same `AnnotationScanCostProbe` scan,
>    `CRATONVM_DBG=jit-compiled` lists **21 methods, two of them `<init>`**
>    (`ConstantUtf8.<init>`, `ConstantClass.<init>`), not "eight, zero
>    `<init>`". `Constant.readConstant`, `ConstantUtf8.getInstance` and
>    `Utility.getClassName` compile now too. So **"the parse is never compiled"
>    and "zero constructors" are both out of date**, and § Root cause — which
>    argues from them — must be re-derived before it is planned against. What
>    is still absent is exactly the frame `--stack-dump-on-timeout` puts on
>    top: `BufferedInputStream.read`, `DataInputStream.readUnsignedByte`,
>    `ClassParser.parse`, `ConstantPool.<init>` — i.e. the `synchronized` /
>    lock-bearing bodies, which is doc 31's subject, not an admission ban.
> 2. **A separate degradation term was found and fixed**, and it is not in this
>    doc's model at all: the cost is not flat, it *rises* within one process.
>    See `fixed-suite-bugs/tomcat/loader-latch-degrades-every-deploy-FIXED.md`.
>    Defining one class through any user-defined loader used to make the whole
>    VM ~1.8x slower permanently. Fixed; worth 1.6x on the probe and **nothing
>    measurable on the test classes**, which is why this doc stays OPEN.

> **Update 2026-08-04 — the "still absent" list above is itself half stale.**
>
> Two of the four frames now compile. `CRATONVM_DBG=jit-compiled` over
> `probes/LoaderStepOneShotProbe.java` (`lib all 6`) lists **27 methods, 3 of
> them constructors**, and includes `ClassParser.parse`; over
> `probes/LoaderStepCostProbe.java` it lists **31**, adding
> `ConstantPool.<init>`, `Constant.<init>` and `JavaClass.<init>`. So of the
> four, only the **JDK's own two** — `BufferedInputStream.read` and
> `DataInputStream.readUnsignedByte` — are reliably never compiled, and those
> are exactly the `synchronized` bodies doc 31 owns. Anything arguing "the
> parse never compiles" is arguing from a census that no longer holds; the
> binding constraint is the per-class *flat* cost (~3 300–3 500 µs against
> HotSpot's ~16 µs), not compiled-vs-interpreted coverage of the BCEL classes.
>
> The loader-latch doc retired the same day, so the degradation term in
> correction 2 is now closed on measurement rather than on a wall-clock
> estimate: a positive-control build with the fix reverted steps **1.60x** at
> the first class definition and the shipped build is flat, and the "~1.5x
> second-loader step" that doc carried as an open residual **does not exist**
> and has been withdrawn.

## Untaken levers

Recorded here rather than lost when the loader-latch doc retired. Neither is
the binding constraint on this doc's classes; both are structurally real.

* **`invokespecial` call sites stop feeding the tiered manager once cached.**
  `vm/src/runtime/interpreter/dispatch_virtual.rs`'s invocation-counter block
  opens with `if !is_special`, and it gates *counting* as well as promotion —
  so once a constructor / private / `super` call site is in the inline cache it
  no longer increments the JIT invocation counter. The uncached route in
  `vm/src/runtime/interpreter.rs` still counts every method regardless of
  opcode, which is why the census above finds constructors compiled anyway, so
  the obvious framing ("constructors are invisible to tier-up") is **not**
  what the code does. Splitting counting from promotion is the small change;
  what makes it a project rather than a fix is the blast radius — widening
  which methods reach the optimizing tier moves work off the single-pass
  backend, whose loop lowerings have no IR-tier equivalent, so it needs
  `regression-suite/perf/c2-reach.sh` plus a CratonBench pass before it can
  land.
* **`new` has no per-call-site class-resolution cache.**
  `opcodes.rs`'s `Instruction::New` re-resolves its constant-pool entry on
  every execution, unlike `put_field`/`put_method`. This is what amplified the
  loader latch into a VM-wide 1.6x; with the latch fixed it is no longer a
  step, but it is still a per-`new` cost that a constant-pool parse pays once
  per entry.

## Symptom

`org.apache.catalina.manager.TestManagerWebapp` fails 2 of its 3 methods on
timing alone:

* `testBug57700` — `SocketTimeoutException: Read timed out` at
  `TestManagerWebapp.java:571`. The `GET /manager/text/deploy` it is waiting on
  runs longer than the client's 30 s read timeout. The deploy itself completes,
  just far too late: `HostConfig` logs `Deployment of web application directory
  [.../bug57700] has finished in [139,999] ms` against HotSpot's `[2 113] ms`.
* `testDeploy` — bare `assertTrue` at `TestManagerWebapp.java:436`, which
  asserts `/manager/text/list` contains `/examples:running`. The preceding
  reload is still in flight when `list` is served: CratonVM redeploys
  `examples` in `[13,312] ms` against HotSpot's `[402] ms`.

`testServlets` passes. Nothing is wrong with the deployed webapp — every deploy
completes correctly, it just misses the test's clock.

## Where the time goes

`--stack-dump-on-timeout=75` over the failing run, 8248 samples of the
serving thread:

| frame | samples | share |
|---|---|---|
| `java/io/BufferedInputStream.read([BII)I` | 7780 | 94.3% |
| `tomcat/util/bcel/classfile/ConstantPool.getConstant` | 388 | 4.7% |
| `tomcat/util/bcel/classfile/AnnotationEntry.<init>` | 36 | 0.4% |
| everything else | 44 | 0.5% |

all under
`ContextConfig.processAnnotationsJar -> processAnnotationsStream`, i.e. Tomcat
scanning every `.class` in the webapp's JARs for annotations.

`probes/AnnotationScanCostProbe.java` replicates that loop standalone — walk
each `.class` entry of a JAR and run Tomcat's own
`new ClassParser(is).parse()` — and separates reading from parsing:

| | HotSpot | CratonVM | ratio |
|---|---|---|---|
| read the entries (`taglibs-standard-impl`, 130 classes) | 3.1 ms | 11.3 ms | 3.6x |
| **BCEL-parse them** | **2.1 ms** | **481.7 ms** | **226x** |
| per class | 16.4 µs | 3705 µs | 226x |

**Re-verified on merged dev `36df168e4`** (29 commits later, including the x64
backend split), three interleaved rounds per VM back-to-back on the same host
state, `taglibs-standard-impl` parse only:

| round | HotSpot | CratonVM |
|---|---|---|
| 1 | 21.7 µs/class | 5440.1 µs/class |
| 2 | 11.7 µs/class | 5077.3 µs/class |
| 3 | 23.0 µs/class | 4959.6 µs/class |

Median-to-median **234x**. Take the ratio, not the absolute microseconds: this
is a shared, variably-loaded host, and HotSpot's own column swings 2x across
the three rounds because the whole parse is only ~2 ms there.

So the JAR/zip/inflate path is fine (`probes/JarEntryReadCostProbe.java`: 30.5
vs 62.7 MiB/s entry reads, raw `Inflater` 938 vs 1254 MiB/s — both under 2x).
The cost is the per-byte class-file parse.

> **Update 2026-08-04 (later) — three candidate root causes measured and
> FALSIFIED, and the section below is wrong about the mechanism.** See
> § What this is NOT — measured, which supersedes both this section and
> § Root cause. Short version: the cost is **general interpreter throughput**,
> not any single gate, and no tier-up lever moves it. `--nojit` is now measured
> *faster* than the default on a quiet host, so compiled code contributes
> nothing here at all.

## Where the cost is, decomposed (2026-08-04)

> This section supersedes § Root cause below on the question of *what* is slow.
> That section's shape — "this code never got compiled" — survives, but it
> names the wrong code, and the difference decides which fix is worth building.
>
> ⚠️ **The per-byte model below does not describe the real parse.** These
> stages are *synthetic* per-byte loops written by the probe; `ClassParser`
> itself reads in BULK. Counted with `--dump-native-registry` over the real
> scan (156 classes): 12,643 `DataInputStream.readUnsignedShort`, 4,768
> `readInt`, 938 bulk `ByteArrayInputStream.read([BII)` — and ~35k native calls
> in total, which cannot account for ~400 ms. `readUnsignedByte` does not even
> reach the top of the list. Do not plan against "~950 ns per byte".

`probes/AnnotationScanSplitProbe.java` runs five stages over the **same
in-memory class bytes**, each loop inlined into a named static method (never a
lambda — see the harness note in that file). 156 classes, 328 KiB,
steady-state round, ns/byte:

| stage | what it does | HotSpot | CratonVM |
|---|---|---|---|
| `parseMem` | `ClassParser.parse()` — construction + I/O chain | 4.8 | **982** |
| `readBytes` | `DataInputStream.readUnsignedByte()` per byte, **constructing nothing** | 0.3 | **971** |
| `readRaw` | `ByteArrayInputStream.read()` per byte — one layer less | 0.5 | **376** |

**`readBytes` alone is ~99% of `parseMem`.** Reading the bytes one at a time,
allocating nothing and parsing nothing, costs essentially the whole scan. So:

* it is **not** object construction, and not the constructor-compilation story
  the § below builds on;
* it is **not** class resolution or `new` — a per-call-site class-resolution
  cache was scoped and then dropped on the strength of this measurement;
* it is **not** the jar/inflate layer — `parseMem` (from a `byte[]`) matches
  `parse` (from the jar entry stream) to within noise.

It is the **per-byte I/O call chain**: ~376 ns for one
`ByteArrayInputStream.read()` (a one-line `synchronized` method) and ~600 ns
more for the `DataInputStream.readUnsignedByte()` wrapper, against HotSpot's
~0.5 ns. That is exactly the frame the stack dump named all along, and it puts
this doc in
`../../internal/fixed-suite-bugs/tomcat/31-synchronized-code-never-jit-compiled-FIXED.md`'s
territory plus the VM-wide per-call floor — not in admission-gate territory.

⚠️ The same probe **SIGSEGVs on CratonVM** in its `arrayRead` stage on the real
Tomcat classpath — a separate, deterministic JIT miscompile:
[`../jit/annotation-scan-arrayread-sigsegv.md`](../jit/annotation-scan-arrayread-sigsegv.md).

## Root cause: the parse is never compiled

`--nojit` costs the **same** as the default — on both the original build and
merged dev `36df168e4`:

| | JIT on | `--nojit` |
|---|---|---|
| `taglibs-standard-impl` parse (first measurement) | 444.9 ms | 437.8 ms |
| `taglibs-standard-spec` parse (first measurement) | 87.0 ms | 92.1 ms |
| `taglibs-standard-impl` parse (merged `36df168e4`) | 702.7 ms | 708.3 ms |
| `taglibs-standard-spec` parse (merged `36df168e4`) | 181.8 ms | 155.1 ms |

`CRATONVM_DBG=jit-compiled` over the whole scan (468 class parses) lists
**eight** compiled methods in total:

```
java/io/BufferedInputStream.close()V
java/io/FilterInputStream.read([B)I
org/apache/tomcat/util/bcel/classfile/ConstantClass.getTag()B
org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant(I)…
org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant(IB)…
org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant(ILjava/lang/Class;)…
org/apache/tomcat/util/bcel/classfile/ConstantUtf8.getTag()B
org/apache/tomcat/util/bcel/classfile/Utility.compactClassName(…)
```

Byte-for-byte the same list on merged dev `36df168e4`, still with a
`grep -c '<init>'` of **0**.

**Zero `<init>` methods** — in a workload whose whole shape is "construct one
object per constant-pool entry". `ClassParser.parse`, `ConstantPool.<init>`,
`Constant.readConstant`, `ConstantUtf8.<init>`, `AnnotationEntry.<init>`,
`DataInputStream.readUnsignedByte` and `BufferedInputStream.read()` are all
absent, and `CRATONVM_DBG=jit-method-stats` reports only 6 distinct methods
tracked with `hot_but_stuck_in_interpreter=0` — the tiering manager never sees
them at all, so they do not even register as stuck.

`probes/SingleByteReadCostProbe.java` prices the layers the parser sits on
(loops in named static methods, called directly — see the harness note in that
file, it matters). On merged dev `36df168e4`:

| operation | HotSpot | CratonVM |
|---|---|---|
| static call returning a field | 0.0 ns | 594.8 ns |
| `ReentrantLock` lock/unlock, uncontended | 15.2 ns | 18189.5 ns |
| `synchronized` enter/exit, uncontended | 4.8 ns | 1365.0 ns |
| `ByteArrayInputStream.read()` | 0.3 ns | 972.0 ns |
| `BufferedInputStream.read()` | 18.8 ns | 5424.1 ns |
| `DataInputStream.readUnsignedByte` over `BufferedInputStream` | 19.9 ns | 16412.7 ns |

> ⚠️ **Read this table for the ratios BETWEEN its own rows, not as absolute
> per-op costs, and do not quote its "static call returning a field" row as this
> VM's call floor.** `probes/CallFloorProbe.java` on the *same binary, same
> session* prices compiled call sites at **4.3 ns** (arith, no call), **11.9 ns**
> (invokestatic leaf), 42.5 ns (invokevirtual), 49.3 ns (invokeinterface) — so
> the 594.8 ns here is ~50x what the same operation costs in a probe that is
> definitely running compiled.
>
> Both probes' loops *are* compiled: `CRATONVM_DBG=jit-compiled,osr` shows
> `SingleByteReadCostProbe.floorLoop()J` OSR-compiled and `floor()I` compiled.
> But the OSR trace shows it **re-entering repeatedly** — at i=2000, 3000, 5000,
> 9000, 17000, the per-pc exponential back-off running to its 5-attempt cap —
> which means the compiled body keeps falling back to the interpreter. That is
> an unexplained second effect, plausibly the same family as this doc's, and it
> is why these absolutes are not trustworthy. `AnnotationScanCostProbe` is this
> doc's load-bearing measurement precisely because it times Tomcat's own code
> with no harness loop of ours in the middle.

The `CallFloorProbe` contrast is the useful part: **when this VM compiles a
method it is within a few x of HotSpot.** The 234x is "this code never got
compiled", not "the compiler emits bad code".

## What this is NOT — measured (2026-08-04, branch `perf/annotation-scan-monitor-wall-20260804`)

Everything in this section was measured on a **quiet host** (load ≈ 6–7; see
§ Measuring this at all) against the real `AnnotationScanCostProbe`, not a
microbenchmark. Baseline for all rows: **HotSpot ≈ 8 µs/class, CratonVM
≈ 1950 µs/class ⇒ ≈ 240x**, which reproduces this doc's 226–234x exactly.

**Not the monitor / `synchronized` cost.** Microbenchmarks are seductive here:
`SingleByteReadCostProbe` prices an uncontended `synchronized` round trip at
605.9 ns against HotSpot's 2.6 ns — 233x, temptingly equal to the headline
ratio. It is a coincidence. In the real scan's profile the only monitor symbol
that appears at all is `complete_jmx_monitor_enter`, at 1.47%.

**Not the `ACC_SYNCHRONIZED` JIT-admission gate.** `ByteArrayInputStream.read()`
genuinely never compiles — `jit_bridge.rs` rejected every `ACC_SYNCHRONIZED`
method on the invocation-counter path, *before* it was ever counted, which is
why `jit-method-stats` reported it neither compiled nor
`hot_but_stuck_in_interpreter`. Admitting them (`CRATONVM_JIT=sync-methods`,
default-off) changes this workload by **nothing**: 2237/2064 → 2140/2096
µs/class. The gap is real and worth closing on its own merits; it is not this.

**Not the outer loops failing to tier up** — though that gap is real too, and
is the most interesting negative result here. `ConstantPool.<init>`,
`ClassParser.readFields` and `readMethods` are invisible to *both* tier-up
counters: the method counter accumulates globally but they run once per class
(468 calls, under the 500 threshold), and the OSR trigger reads
`Frame::backward_count`, which is **per-frame and reset on every invocation**,
so a ~74-iteration constant-pool loop never approaches the 1000-back-edge
threshold *within one frame* — permanently, not as a warm-up artifact. The
stock scan reports `osr=0`: not one OSR body in the entire run.
`CRATONVM_JIT=loop-work-tierup` (default-off) fixes that — `ConstantPool.<init>`
compiles, tracked methods 9 → 10 — and buys **2–3%, inside the noise**.

**Not a field-resolution-cache miss either.** `resolve_field_ref_loader_aware`
does full symbolic work on every access *including a cache hit* — two `String`
allocations, two extra `class_manager.read()`s and a whole
`resolve_class_loader_aware` — purely to revalidate the entry it already holds.
Short-circuiting that for non-loader-sensitive callers
(`CRATONVM_JIT=field-cache-fastpath`, default-off) is worth **nothing**
measurable: off 1894–2047, on 1919–2019 µs/class over four interleaved passes.
(The waste is real and worth removing on its own merits; it is ~3% of a 250x
gap. NB I did not independently confirm the fast path fires — the available
counters do not distinguish it — so read this as "no measurable effect", not as
"the fast path was exercised and did not help".)

**So it is not a gate at all — it is interpreter throughput.** The decisive
measurement: on a quiet host `--nojit` is *faster* than the default
(1911/1868 vs 1978/1952 µs/class). Compiled code contributes nothing to this
workload; compilation overhead slightly outweighs it. The `perf` profile is
correspondingly flat — `execute_frame_from_index` 11.6%, then a long tail at
1–4% each (`is_object_address` 4.2, `execute_instruction` 3.6, `memcmp` 3.5,
`resolve_field_ref_loader_aware` 3.3, `invoke_on_class_shared_inner` 3.1,
`execute_invokevirtual_cached` 2.8, `load_class_concurrent` 2.2,
`slot_for_exact` 1.6, `complete_jmx_monitor_enter` 1.5) — no hotspot to remove.

### The number that actually sizes this: interpreter vs interpreter

`CallFloorProbe` under `HotSpot -Xint` against `CratonVM --nojit` removes the
JIT from both sides and prices the interpreters directly (ns/op):

| body | HotSpot `-Xint` | CratonVM `--nojit` | ratio |
|---|---|---|---|
| arith (no call) | 17.2 | 176.1 | 10x |
| + invokestatic leaf | 21.1 | 435.7 | **21x** |
| + invokevirtual leaf | 17.2 | 666.0 | **39x** |
| + invokeinterface leaf | 17.0 | 679.7 | **40x** |
| + `String.length()` | 26.6 | 1105.1 | **42x** |

Read the *increments*, not the absolutes: adding one call costs HotSpot's
interpreter ~4 ns and CratonVM's **~260 ns (static) to ~490 ns (virtual)** —
**65–120x**. Pure arithmetic is only 10x. So this VM's interpreter is
respectable at straight-line bytecode and catastrophic at **invoke**, and the
BCEL parse is invoke-dense (one object per constant-pool entry, getters
throughout). That, not any one symbol, is the 226x.

Scale, from `CRATONVM_DBG=hotpath-counts` over the same scan: ~2.0M bytecodes
executed, ~1.2M instance-field accesses, 201k method-ref resolutions — about
5,300 bytecodes per class at ~385 ns each.

**What would actually move this** is the interpreted invoke path itself:
`try_stackless_invoke` alone runs ~34 string comparisons per call (several of
them `class_name.contains(...)` *substring searches* — that is the
`is_contained_in` in the profile), plus a native-registry probe, a
class-manager read lock, `Arc` clone/drop traffic for code+name+descriptor, and
frame push/pop. Fixing that is an interpreter-dispatch project — inline caches,
a resolved constant pool, per-call-site precomputed flags — not a point fix,
and it is the only thing that gets 240x anywhere near the ~5x exit criterion.

**Consequence for the exit criteria below: they are not reachable by tiering
work.** ~240x against an interpreter that the JIT cannot help is a
general-throughput problem. Anyone picking this up should either attack
interpreter dispatch cost broadly, or re-scope the exit criteria.

Still true from the original triage:

* Not the `seek0`/`ExpandWar` defect — that is fixed and verified separately;
  the `ExpandWar` error no longer appears in these runs.
* Not JAR/zip/inflate throughput (under 2x, measured above).
* Not GC and not a hang — the deploys complete, just late.

## Measuring this at all

The Azure build host is shared, and during this investigation its load ran
between 6 and 178. The **same binary and configuration** measured 2237 and
14789 µs/class an hour apart, and the HotSpot column swung 13.5 → 99.0 µs/class
across three interleaved rounds. Any number in this doc taken at load > 10 is
noise. Check `/proc/loadavg` first; interleave the arms in both directions; and
prefer CratonVM-vs-CratonVM A/B over the cross-VM ratio.

Two levers here are **partially inert**, which is worse than useless because
they read as clean negatives:

* `CRATONVM_JIT=threshold=N` moves the *counting-site* threshold but **not**
  the tiered manager's own — `jit-method-stats` still prints
  `c1_threshold=500` at `threshold=10`.
* Crossing the invocation threshold does not by itself nominate anything: the
  only site that acts on the counter is the dispatch site, and it tests
  `cnt == threshold || (cnt - threshold) % 64 == 0` against the value **its
  own** increment returned. `CRATONVM_DBG=loop-work` was added to make this
  visible — it caught `ConstantPool.<init>` sitting at a count of **1149**,
  more than twice the threshold, still never nominated.

## Prior art

This is the same wall
`../../internal/tomcat/04-embedded-server-throughput-wall-CLOSED.md`
measured on 2026-07-27 (it recorded 13–16 µs per byte for the identical
`DataInputStream`/`BufferedInputStream` operation; today's
`ByteReadCostProbe` reads 15.0 µs) and handed to
`31-synchronized-code-never-jit-compiled-FIXED.md`. Doc 31's fix landed on
2026-07-28 and did not move this path. It is also the residual that
`fixed-suite-bugs/managerwebapp-deploy-bare-assertion-FIXED.md`
retired against in July, naming these same two test methods.

The admission bans that plausibly own it are catalogued in the retired
`30-hot-loop-jit-admission-bans-testmethodperformance` write-up (RBC.6
`local_handler_reads_unsafe_local`, RBC.7 `invokedynamic` OSR denial, and the
`<init>` complexity ban). **They are load-bearing** — each closed a real
silent-wrong-results bug — so lifting any of them needs the corruption
regression suite plus a full Tomcat+Spring+Hibernate soak, not a local
measurement.

## Reproduction

Standalone, no Tomcat startup, ~2 s per run:

```bash
cd C:/craton/CratonVM/apps/tomcat
javac -cp "$(cat .suite/cp.txt)" -d /tmp/probeout probes/AnnotationScanCostProbe.java
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g \
  -cp "/tmp/probeout;$(cat .suite/cp.txt)" \
  AnnotationScanCostProbe output/build/webapps/examples/WEB-INF/lib
```

Full class (≈230 s CratonVM, ≈12 s HotSpot):

```bash
pwsh apps/tomcat-suite-runner/run-one.ps1 -Vm craton -Exe <cratonvm.exe> -Class org.apache.catalina.manager.TestManagerWebapp
```

## Exit criteria

> ⚠️ **Not reachable by tiering work** — see § What this is NOT — measured.
> Every JIT-admission and tier-up lever tried on 2026-08-04 moved this by ≤3%,
> and `--nojit` is *faster* than the default, so the remaining distance is
> interpreter throughput. Closing this doc means either a broad interpreter
> dispatch improvement or a re-scoped criterion.

`AnnotationScanCostProbe` within ~5x of HotSpot per class, which should bring
the `examples` redeploy under the ~1 s the `list` assertion needs and the
`bug57700` deploy under the client's 30 s read timeout. Both test methods then
pass without touching the test.
