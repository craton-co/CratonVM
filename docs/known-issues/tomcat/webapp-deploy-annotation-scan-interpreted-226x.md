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

So the JAR/zip/inflate path is fine (`probes/JarEntryReadCostProbe.java`: 30.5
vs 62.7 MiB/s entry reads, raw `Inflater` 938 vs 1254 MiB/s — both under 2x).
The cost is the per-byte class-file parse.

## Root cause: the parse is never compiled

`--nojit` costs the **same** as the default:

| | JIT on | `--nojit` |
|---|---|---|
| `taglibs-standard-impl` parse | 444.9 ms | 437.8 ms |
| `taglibs-standard-spec` parse | 87.0 ms | 92.1 ms |

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

**Zero `<init>` methods** — in a workload whose whole shape is "construct one
object per constant-pool entry". `ClassParser.parse`, `ConstantPool.<init>`,
`Constant.readConstant`, `ConstantUtf8.<init>`, `AnnotationEntry.<init>`,
`DataInputStream.readUnsignedByte` and `BufferedInputStream.read()` are all
absent, and `CRATONVM_DBG=jit-method-stats` reports only 6 distinct methods
tracked with `hot_but_stuck_in_interpreter=0` — the tiering manager never sees
them at all, so they do not even register as stuck.

`probes/SingleByteReadCostProbe.java` prices the layers the parser sits on
(loops in named static methods, called directly — see the harness note in that
file, it matters):

| operation | HotSpot | CratonVM | ratio |
|---|---|---|---|
| static call returning a field | 0.0 ns | 589.9 ns | — |
| `ReentrantLock` lock/unlock, uncontended | 15.2 ns | 17254.2 ns | 1135x |
| `synchronized` enter/exit, uncontended | 4.8 ns | 1189.4 ns | 248x |
| `ByteArrayInputStream.read()` | 0.3 ns | 864.3 ns | — |
| `BufferedInputStream.read()` | 18.8 ns | 5062.1 ns | 269x |
| `DataInputStream.readUnsignedByte` over `BufferedInputStream` | 19.9 ns | 15422.3 ns | 775x |

For contrast, `probes/CallFloorProbe.java` on the same binary shows compiled
call sites at 9.8–41.7 ns/op — i.e. **when this VM compiles a method it is
within a few x of HotSpot**; the 226x is entirely "this code never got
compiled".

## What this is NOT

* Not the `seek0`/`ExpandWar` defect — that is fixed and verified separately;
  the `ExpandWar` error no longer appears in these runs.
* Not JAR/zip/inflate throughput (under 2x, measured above).
* Not GC and not a hang — the deploys complete, just late.

## Prior art

This is the same wall
`docs/internal/fixed-suite-bugs/tomcat/04-embedded-server-throughput-wall-CLOSED.md`
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
