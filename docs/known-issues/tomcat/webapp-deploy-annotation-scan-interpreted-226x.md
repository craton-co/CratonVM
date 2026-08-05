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
(`CRATONVM_JIT=field-cache-fastpath`, default-off) measured **nothing**: off
1894–2047, on 1919–2019 µs/class over four interleaved passes.

> **RETRACTED 2026-08-04 — that lever was INERT and the null result says
> nothing.** It gated on `should_use_loader_initiated_resolution`, which begins
> `if loader_aware_resolution() { return true; }` — and that flag is **default
> ON** (`classloading/src/class_manager.rs:503`, consolidated there precisely so
> the three copies could not drift). The predicate is therefore unconditionally
> true and the fast path could never execute. The stated caveat ("I did not
> confirm the fast path fires") was the tell; it should have been a blocker, not
> a footnote.
>
> The predicate that actually splits the cases is `loader_sensitive`, which
> additionally requires the referencing class's loader to be `UserDefined`. The
> replacement (`CRATONVM_JIT=field-site-cache`) uses that one and ships a
> `CRATONVM_DBG=field-site` counter, so "did the lever fire" is answerable
> before anything is timed. This is the fifth inert-lever incident in this
> investigation; a lever now has to prove it fired before it is allowed a
> timing number.

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

**What would actually move this** is the interpreted invoke and field paths.
Fixing them is an interpreter-dispatch project — inline caches, a resolved
constant pool, per-call-site precomputed flags — not a point fix, and it is the
only thing that gets 240x anywhere near the ~5x exit criterion.

#### Correction: it is not `try_stackless_invoke` (2026-08-04)

An earlier revision of this section named `try_stackless_invoke`'s ~34 per-call
string comparisons as the thing to fix. **That was wrong, and the way it was
wrong is worth keeping.** `CRATONVM_DBG=invokestats` over the scan:

```
[invokestats] cache_hit=600001 cache_miss=1784 vtable_fast=2672 slow_path=3970
```

The monomorphic inline cache is **99.7% warm**. `try_stackless_invoke` is the
cache-*miss* path; it runs on roughly 0.3% of invokes, so its comparison count
is irrelevant no matter how large. The claim was inferred from reading the
source rather than from asking how often the function executes — the same
mistake, in a different costume, as the four falsified root causes above.

The string comparisons per invoke are real, but they are on the **hit** path:
`intercept_force_registered_native_cached` runs a sequence of
`(method_name, method_descriptor)` matches on every inline-cache hit that
dispatches bytecode. That is a per-call-site precomputable question and is
where the "precomputed flags" half of the project belongs.

#### The clusters, by mechanism

Re-profiled at a 0.35% floor on a quiet host (2039.7 / 1976.2 µs/class):

| cluster | share | mechanism |
|---|---|---|
| dispatch loop | ~13.5% | `execute_frame_from_index` 8.92, `execute_instruction` 3.38, `execute` 1.19 |
| **field resolution** | **~12%** | `resolve_field_ref_loader_aware` 3.51, `load_class_concurrent` 2.59, `resolve_class_loader_aware` 2.02, plus its share of `memcmp` 4.30, `sip::Hasher` 0.48, `_mi_page_malloc_zero` 1.10 / `mi_free` 0.79 |
| invoke + frame | ~12% | `execute_invokevirtual_cached` 2.90, `pop_and_recycle_frame` 1.62, **`drop_in_place<Option<(Arc.., Arc<str>, Arc<str>, usize)>>` 1.54**, `InvokeCache::get` 1.23, `Frame::new_pooled_cached` 1.05 |
| native registry | ~4.2% | `slot_for_exact` 2.50, `should_force_registered_native_over_bytecode` 0.75, `slot_index_for_key` 0.53, `intercept_force_registered_native_cached` 0.40 |
| heap checks | ~6.5% | `is_object_address` 3.78, `record_object_ref_payload_slow` 1.05 |

Two of those have an identified, removable mechanism rather than just a name:

* **Field resolution.** `resolve_field_ref_loader_aware` re-derives the
  field-*owning class* from its name on **every** access, resolution-cache hit
  included: two `String` allocations, three `class_manager` read acquisitions
  and a full `resolve_class_loader_aware`. Only the second half of the answer
  (locate the field in the owner) is memoized. ~1.2M accesses.
* **That 1.54% `drop_in_place`.** It is `resolve_method_ref`'s four-value return
  being dropped. `pop_coerced_invoke_args_virtual` / `_static` call it on the
  inline-cache **hit** path for two of those four values — the descriptor and
  the parameter count — paying a `resolution_cache` read lock, a hash probe and
  three `Arc<str>` clone/drop pairs per invoke to get one string and one
  integer.

Both are answered by the same thing: a per-thread, epoch-validated resolved
constant pool (`vm/src/runtime/interpreter/site_cache.rs`), behind
`CRATONVM_JIT=field-site-cache` and `CRATONVM_JIT=method-site-cache`.

#### Real-application validation: 351 Spring Boot test classes (2026-08-05)

The regression suite and the two dedicated vectors do not answer "is this safe
on a real, loader-heavy application". Spring Boot's own unit tests do. Corpus:
351 compiled test classes from `core/spring-boot`, driven one process per class
through a JUnit Platform launcher harness, both arms.

**Result: 346/351 vs 347/351 rc=0, and all 5 divergent classes are FLAKY IN BOTH
ARMS**, not cache defects:

| class | evidence |
|---|---|
| `SpringApplicationTests` | 5 reps/arm: off `failed=0,0,8,0,0`; on `0,0,4,0,0` — flakes in both |
| `SpringApplicationBuilderTests` | 15 reps/arm, **alternating**: off bad 3/15, on bad 4/15 |
| `StringToPeriodConverterTests` | 5 reps/arm: 0/5 both; the original `containersFailed=1` was in the OFF arm |
| `DefaultSslManagerBundleTests` | 5 reps/arm: 0/5 both; original `failed=2` was in the OFF arm |
| `ThreadPoolTaskSchedulerBuilderTests` | 5 reps/arm: 0/5 both |

The divergences were **bidirectional** — 3 classes did better with the cache on,
2 worse — which is the signature of flakiness, not of a defect. A cache serving
a wrong field is one-directional and deterministic.

The lever demonstrably fires on this corpus (`hit=184126` on one test class), so
this is not a vacuous green.

**Two honest caveats.**

* **The 351-class run was blocked, not interleaved** (`off` × 351, then `on` ×
  351). The off arm ran at host load 16–30 and the on arm after load dropped,
  which systematically favours ON for load-sensitive flaky tests — and 3 of the
  5 divergences favoured ON. That is a violation of this repo's own interleaving
  rule; the follow-ups above alternate arms per repetition, which is what makes
  the 3/15-vs-4/15 comparison trustworthy.
* **This does NOT validate `field-site-cache-loader`.** Every site in this
  corpus is loader-blind (`reject_loader` ≈ 0 — the tests run off a plain
  classpath), so the loader arm was inert here. It remains unvalidated on real
  loader-heavy code, which is exactly where it is supposed to matter.

#### Sizing: 1024 slots thrash on broad application code

The hit rate that makes the scan number possible does **not** generalise:

| workload | hit | miss | rate |
|---|---|---|---|
| Tomcat annotation scan | 1,329,432 | 1,234 | **99.9%** |
| `SpringApplicationShutdownHookTests` | 184,126 | 166,035 | **53%** |

Nearly every Spring Boot miss causes a fill — the table is thrashing, not
warming. 1024 was sized against a narrow hot loop. `CRATONVM_JIT=field-site-slots=N`
exists to settle this with data rather than a guess; hit rate is
**load-independent**, so it is measurable on this shared host even when timings
are not.

**This is why the default stays OFF.** The scan gain is real and measured, but
the benefit on broad application code is *unmeasured in time* — only its hit
rate is known, and that hit rate says the current size is wrong for that shape.

#### What those two levers are worth — ON THE SCAN (2026-08-05)

The authoritative measurement: real BCEL, real JARs, Azure host, load steady at
**1.39–1.47** for the whole run. Four interleaved passes, arm order reversed on
even passes, HotSpot control every pass. `us/class`, both `parse` readings per
pass:

| arm | pass1 | pass2 | pass3 | pass4 | mean |
|---|---|---|---|---|---|
| off | 1845.2 / 1821.8 | 1884.3 / 1822.1 | 1855.9 / 1807.0 | 1878.5 / 1818.0 | **1841.6** |
| `field-site-cache` | 1623.1 / 1577.6 | 1639.5 / 1592.1 | 1616.0 / 1585.6 | 1632.7 / 1596.8 | **1607.9** |
| + loader arm | 1615.4 / 1595.1 | 1617.7 / 1601.8 | 1614.2 / 1584.5 | 1608.7 / 1583.4 | **1602.6** |
| + `method-site-cache` | 1598.0 / 1585.2 | 1604.2 / 1577.1 | 1606.1 / 1583.9 | 1622.8 / 1589.6 | **1595.9** |
| HotSpot | 7.4 / 6.2 | 6.7 / 6.8 | 8.3 / 7.7 | 7.2 / 6.6 | **7.11** |

**12.7% off the scan for `field-site-cache` alone; 13.3% with all three.** The
separation is total: every one of the 8 `off` readings is 1807–1884, and every
one of the 24 lever readings is 1577–1640. No overlap, in either order, on a
quiet host.

Against HotSpot that is **259x → 224x**. Real, and nowhere near the ~5x exit
criterion — which is the point the rest of this document makes.

Structural check, same run — the lever fires 1.33M times on one scan:

```
off:    field: hit=0        miss=0    | method: hit=0
field:  field: hit=1329432  miss=1234 | method: hit=0
fieldl: field: hit=1329436  miss=1234 | method: hit=0
all:    field: hit=1329434  miss=1234 | method: hit=52566
```

`reject_loader=0` throughout: this probe runs off a plain classpath, so every
site is loader-blind and the loader arm has nothing extra to admit — which is
why it adds only 0.3%. Inside a real webapp deploy, where the scanning code runs
under a user-defined loader, that arm is what keeps the base arm from being
inert; `reject_loader` is the counter that will say so.

`method-site-cache` adds 0.4% on top (1602.6 → 1595.9) — consistent with the
"measures nothing" verdict below, marginally positive rather than negative.

#### The mechanism's own price, in isolation

Measured separately with `probes/SiteCacheCostProbe.java`, which is deliberately
field-saturated, so its ratio is the *mechanism's* headroom and not the scan's.

Structural check first — both levers demonstrably fire, which is the thing the
retracted measurement above never established:

```
off:    field: hit=0         | method: hit=0
field:  field: hit=12900001  | method: hit=0
method: field: hit=0         | method: hit=900043
both:   field: hit=12900001  | method: hit=900043
```

`--nojit`, ns/op, mean of the last two rounds across four passes:

| benchmark | off | field-site-cache | method-site-cache | HotSpot `-Xint` |
|---|---|---|---|---|
| field-heavy | 43,630 | **21,170** (1.9–2.7x per pass) | 49,900 | ~270–440 |
| mixed | 43,331 | **23,470** | 45,850 | ~425–520 |
| native-call-heavy | 5,467 | 5,282 | 5,915 | ~400–480 |

* **`field-site-cache` is worth ~1.9x on interpreted field-heavy code**, and the
  ratio holds in every pass in both orders (1.94, 1.84, 2.69, 1.97). It cuts the
  interpreter-vs-interpreter gap on this shape from ~100–160x to ~50–79x.
* **`method-site-cache` measures nothing.** It removes real work — a
  `resolution_cache` read lock, a hash probe and three `Arc<str>` clone/drop
  pairs per invoke — but that work is small beside the `safe_native_call` funnel
  a native invoke pays anyway (~450 ns/call here). The counter proves it fires
  900k times and it still does not show. Kept, default-OFF, on the same footing
  as the other measured-nothing levers: the waste is real, the payoff is not.

In **default (JIT-on)** mode neither lever shows a reliable difference on this
probe (field-heavy: off ~2,570, field ~2,466, both ~2,233 ns/op, inside the
spread) — the JIT compiles the loop and never touches the interpreter's field
path. That is consistent rather than contradictory: this workload is only
interesting because the annotation scan is a case where **the JIT contributes
nothing** (`--nojit` is *faster* there, measured above), so the scan sits in the
first regime, not the second.

Correctness: 28/28 regression suite green with the levers off and with both
levers plus the loader arm on. Two dedicated vectors — `RFieldSiteCache` (291
checks) and `RMethodSiteCache` (44) — target the silent failure modes
specifically, since every way these caches can be wrong returns a plausible
number rather than throwing.

#### The purpose-built probe hid a regression; independent vectors found it

The first version of the cache held **one** epoch pair for the whole table and
wiped all 1024 slots when either moved. `class_definition_epoch` advances on
*every class definition*, so during start-up and any class-loading burst it
moves constantly — and each field access was then paying an `O(SLOTS)` memset.

`SiteCacheCostProbe` never showed this, because it reaches steady state and
stops defining classes. Six regression vectors that never reach steady state
(~500 ms runs, boot-dominated) did, and they were **uniformly slower** with the
lever on:

| vector | off | on (table-wide wipe) | on (per-entry epochs) |
|---|---|---|---|
| RCollections | 499 | 629 | 664 vs 666 off |
| RStrings | 563 | 621 | 675 vs 628 off |
| RSerial | 572 | 673 | 781 vs 781 off |
| RExceptions | 526 | 623 | 669 vs 698 off |
| RReflect | 552 | 592 | 709 vs 675 off |
| RNumbers | 581 | 679 | 693 vs 713 off |

Holding the epoch pair **per entry** removes the wipe entirely: an epoch change
costs nothing, stale entries miss one at a time and are replaced in place, and a
hit is one array index plus four integer compares on a single cache line. After
that change the six vectors are at parity — the correct outcome for a
boot-dominated run, where the cache should not help and must not hurt.

Two things worth keeping from this:

* **A probe written alongside a fix will tend to exercise the shape the fix is
  good at.** The independent check was what caught it, and it is cheap.
* **A cache's invalidation cost is part of its cost.** An `O(n)` wipe keyed on a
  counter that moves during class loading is not a cache, it is a memset with a
  lookup attached.

Re-measured after the redesign the field arm still lands in the same band, but
that run was taken on a box no longer idle (the `off` column spread 38k–127k
against 37k–48k on the clean run), so **the 1.9x above is the clean-run figure**
and the re-measurement should be read only as confirming direction and
magnitude, not as an independent estimate.

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
>
> **Progress 2026-08-05: 259x → 224x** (12.7–13.3%) from the interpreter's
> resolved constant pool, `CRATONVM_JIT=field-site-cache` (still default-OFF).
> That is the first lever in this investigation to move the number outside
> noise, and it confirms the diagnosis — the win came from deleting per-access
> symbolic work, not from compiling anything. It also sizes what is left: at
> 224x, reaching ~5x needs roughly another 45x, which no cache on the field path
> can supply. The remaining mass is the dispatch loop itself
> (`execute_frame_from_index` + `execute_instruction` ≈ 13.5%), frame push/pop,
> and the per-invoke native-registry and heap-check work — i.e. a genuine
> interpreter rewrite, or a re-scoped criterion.

`AnnotationScanCostProbe` within ~5x of HotSpot per class, which should bring
the `examples` redeploy under the ~1 s the `list` assertion needs and the
`bug57700` deploy under the client's 30 s read timeout. Both test methods then
pass without touching the test.
