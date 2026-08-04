# Webapp deploy: BCEL annotation scan is 226x slower — it never compiles

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | high — blocks `TestManagerWebapp.testDeploy` + `.testBug57700`; makes every deploy-heavy Tomcat class 15–65x slower |
| **HotSpot** | PASS |
| **CratonVM** | FAIL (timing only — no wrong results, no crash) |
| **Discovered** | 2026-08-03, after fixing the `seek0`/`ExpandWar` defect that had been masking it (`docs/internal/fixed-suite-bugs/tomcat/testmanagerwebapp-expandwar-seek0-bad-fd-FIXED.md`) |

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

## What this is NOT

* Not the `seek0`/`ExpandWar` defect — that is fixed and verified separately;
  the `ExpandWar` error no longer appears in these runs.
* Not JAR/zip/inflate throughput (under 2x, measured above).
* Not GC and not a hang — the deploys complete, just late.

## Prior art

This is the same wall
`../../internal/tomcat/04-embedded-server-throughput-wall-CLOSED.md`
measured on 2026-07-27 (it recorded 13–16 µs per byte for the identical
`DataInputStream`/`BufferedInputStream` operation; today's
`ByteReadCostProbe` reads 15.0 µs) and handed to
`31-synchronized-code-never-jit-compiled-FIXED.md`. Doc 31's fix landed on
2026-07-28 and did not move this path. It is also the residual that
`docs/internal/fixed-suite-bugs/managerwebapp-deploy-bare-assertion-FIXED.md`
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

`AnnotationScanCostProbe` within ~5x of HotSpot per class, which should bring
the `examples` redeploy under the ~1 s the `list` assertion needs and the
`bug57700` deploy under the client's 30 s read timeout. Both test methods then
pass without touching the test.
