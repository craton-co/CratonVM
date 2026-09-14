# The H2 corpus failures, with the control arm taken

**2026-08-12.** `P4A-FIRST-CORPUS-RUN-20260812.md` recorded four failures out of
fourteen H2 classes under `--jdk-only` and said plainly that no `--real-jdk`
control had been run, so nothing could be attributed. This document takes that
control and diagnoses the four.

Binary for every CratonVM row below:
`/c/craton/jdkonly-wave2-target/release/cratonvm.exe`. Oracle: HotSpot 25
(`jdk-25.0.3.9-hotspot`), same host, same session.

## 1. The control arm

```
bash regression-suite/corpus/run-corpus.sh run h2 --classes-from <the same 14> \
    --cv /c/craton/jdkonly-wave2-target/release/cratonvm.exe \
    --mode real-jdk --timeout 420
```

```
corpus=h2 mode=jdk-only   AGREE=10  DIVERGE=2  CV-BROKEN=2  UNADJUDICATED=0   (prior run)
corpus=h2 mode=real-jdk   AGREE=11  DIVERGE=1  CV-BROKEN=2  UNADJUDICATED=0   (this run)
```

**The caps differ between the two runs — 200 s vs 420 s — so the raw table
below is confounded on every TIMEOUT row.** §3b works that out; the
`attribution` column already carries the corrected reading.

| class | `--jdk-only` (cap 200 s) | `--real-jdk` (cap 420 s) | attribution *after* §2–§3 |
|---|---|---|---|
| `TestAuthentication` | AGREE | AGREE | |
| `TestAlter` | AGREE | AGREE | |
| **`TestAlterSchemaRename`** | **DIVERGE** | **AGREE** | strict-mode divergence, **intermittent failure** (§2) |
| `TestAlterTableNotFound` | AGREE | AGREE | |
| **`TestAnalyzeTableTx`** | **CV-TIMEOUT** | **AGREE** | throughput failure; mode-dependence **unresolved** (§3b) |
| `TestAutoRecompile` | AGREE | AGREE | |
| **`TestBackup`** | AGREE | **DIVERGE** | **harness artefact**, green in all 3 arms alone (§3c) |
| `TestBigDb` | AGREE | AGREE | |
| `TestBigResult` | AGREE | AGREE | 118 s vs 273 s — same workload, host drift |
| **`TestCases`** | **DIVERGE** | **CV-TIMEOUT** | **both modes**, different faces |
| `TestCheckpoint` | AGREE | AGREE | |
| `TestClearCacheAfterDdl` | AGREE | AGREE | |
| `TestCluster` | AGREE | AGREE | |
| **`TestCompatibility`** | **CV-TIMEOUT** | **CV-TIMEOUT** | **both modes — a real hang** (§3a) |

**The control demolishes most of the original attribution, and not in the
direction anyone expected.** Of the four failures the first run reported:

* `TestCompatibility` is a genuine **hang** that happens in **both** modes.
* `TestCases` fails in **both** modes (throw under strict, timeout under real).
* `TestAnalyzeTableTx` is neither crash nor hang but a **throughput** failure.
  The two corpus runs used different caps so their verdicts are not
  comparable; a matched-budget re-run under `--jdk-only` still did not finish,
  so it is *not* simply the cap either. **Unresolved** — one observation per
  arm on a host with 2.3x drift is not a mode attribution (§3b).
* `TestAlterSchemaRename` is the only one with a real strict-mode component,
  and even there the *failure* is intermittent (0 in 8 on re-run) while only
  the underlying `ServiceLoader` divergence (§2a) is deterministic.

Plus one the control added and then retracted: `TestBackup`, a false
divergence manufactured by the shared working directory.

This matches the independent finding in
`internal/jdk-only/h2-under-jdk-only-three-arm-triage-20260805.md`, which
already listed `TestCases` under "both modes — pre-existing, NOT strict-mode
defects", and which warned in almost these words that a per-class wall-clock
bound on this host "is measuring the host".

Note also that `TestCases` changes *shape* between modes: under `--jdk-only` it
throws in 14 s, under `--real-jdk` it runs past the 420 s wall. A fix to the
strict-mode defect in §2 would move `TestCases` from the first face to the
second, not to green.

### A caveat that applies to every row above

**Both runs executed all fourteen classes in one shared working directory, and
`run-corpus.sh` does not isolate them.** `cmd_run` computes
`wd="$(corpus_workdir "$CORPUS_ROOT")"` at `run-corpus.sh:426` and then never
uses `$wd` — no `cd`, no `--workdir`. Both arms therefore run in whatever
directory the driver was *invoked* from, which is why an untracked `data/`
appeared in the git worktree during these runs. H2's `TestBase.BASE_TEST_DIR`
is `./data` and its `error.lock` is cwd-relative, so classes share database
state with each other, with the previous run, and across modes. The 2026-08-05
triage already knew this and reproduces one class with `cd $(mktemp -d)` for
exactly this reason.

That is the most likely explanation for the one row that got *worse* in the
control, `TestBackup` (`MVStoreException: Chunk 2 not found`, i.e. a corrupt
carried-over store), and it means no row here is a clean per-class
measurement. §3c re-runs `TestBackup` in a fresh cwd to check.


## 2. The two DIVERGE are one *chain*: a deterministic strict-mode divergence, and an intermittent failure behind it

Read §2c before quoting §2a as "the cause of the DIVERGE". The chain has two
links and only the first is deterministic:

1. **Deterministic, strict-mode-only.** Under `--jdk-only`,
   `ToolProvider.getSystemJavaCompiler()` returns `null`, so H2 compiles
   `CREATE ALIAS` bodies with a *different compiler* than HotSpot uses. This
   reproduces every time (§2a) and is a genuine observable divergence in its
   own right.
2. **Intermittent.** That fallback path then sometimes — not always — dies on
   a `ClassId(0)` receiver (§2c). A fresh-cwd re-run of
   `TestAlterSchemaRename` under `--jdk-only` completed successfully *with
   `getSystemJavaCompiler()` still returning `null`*, so link 1 alone does not
   fail the test.

Fixing link 1 is still the right move — it puts H2 back on HotSpot's code path
and removes the exposure — but it should not be reported as "the DIVERGE fix"
until link 2 is understood.


`TestAlterSchemaRename` and `TestCases` produce the *same* first divergent
marker. HotSpot reaches the terminal marker; CratonVM throws:

```
--- HotSpot ---
CORPUS-END org.h2.test.db.TestCases completed=true
--- CratonVM ---
CORPUS-THROW org.h2.test.db.TestCases org.h2.jdbc.JdbcSQLNonTransientException
```

Both throws are raised by the same H2 statement shape — `CREATE ALIAS ... AS
<java source>`, which makes H2 compile Java at runtime — and both carry the
same cause:

```
Caused by: java.lang.NoSuchMethodError: java.lang.Object.flush()V
	at java.io.OutputStreamWriter.flush(OutputStreamWriter.java:249)
	at java.io.PrintWriter.flush(PrintWriter.java:380)
	at com.sun.tools.javac.util.Log.flush(Log.java:487)
	at com.sun.tools.javac.main.JavaCompiler.close(JavaCompiler.java:1935)
	at com.sun.tools.javac.Main.compile(Main.java:66)
	at org.h2.util.SourceCompiler.javacSun(SourceCompiler.java:417)
```

The `javacSun` frame is the tell: it is **not the frame HotSpot runs**.
`org/h2/util/SourceCompiler.java:164` chooses between two compilers:

```java
if (JAVA_COMPILER != null && useJavaSystemCompiler) {
    classInstance = javaxToolsJavac(packageName, className, s);   // HotSpot goes here
} else {
    byte[] data = javacCompile(packageName, className, s);        // CratonVM goes here
}
```

`JAVA_COMPILER` is `ToolProvider.getSystemJavaCompiler()`, cached in a static
initialiser. Measured directly (`ToolProbe`, one process each):

| arm | `ToolProvider.getSystemJavaCompiler()` | H2 branch taken |
|---|---|---|
| HotSpot 25 | `com.sun.tools.javac.api.JavacTool` | `javaxToolsJavac` |
| CratonVM `--real-jdk` | `com.sun.tools.javac.api.JavacTool` | `javaxToolsJavac` |
| CratonVM `--jdk-only` | **`null`** | `javacSun` (legacy reflection) |

So the divergence is not "H2 compiles Java badly on CratonVM". It is that under
`--jdk-only` H2 is pushed down a fallback path HotSpot never takes — and that
path is where the intermittent second defect of §2c lives. The branch choice
itself is 100% reproducible; whether the branch then fails is not.

### 2a. Root cause: module-declared services are invisible until `ModuleLayer.boot()` is called

`ToolProvider.getSystemJavaCompiler()` is nothing but a `ServiceLoader` lookup
(`java.compiler/javax/tools/ToolProvider.java`, JDK 25 `lib/src.zip`):

```java
ServiceLoader<T> sl = ServiceLoader.load(clazz, ClassLoader.getSystemClassLoader());
for (T tool : sl) {
    if (Objects.equals(tool.getClass().getModule().getName(), moduleName)) return tool;
}
return null;
```

Under `--jdk-only` that loop finds nothing. The scope of the failure is exact
and was measured, not inferred:

| service | declared by | HotSpot | `--real-jdk` | `--jdk-only` |
|---|---|---|---|---|
| `MySvc` (a `../../../apps/META-INF/services` file on the classpath) | classpath | 1 | 1 | **1** |
| `java.nio.file.spi.FileSystemProvider` | module `provides` | 2 | 2 | **0** |
| `java.util.spi.ToolProvider` | module `provides` | 9 | 9 | **0** |
| `javax.tools.JavaCompiler` | module `provides` | 1 | 1 | **0** |
| `javax.tools.Tool` | module `provides` | 3 | 3 | **0** |
| `javax.tools.DocumentationTool` | module `provides` | 1 | 1 | **0** |

**Classpath `../../../apps/META-INF/services` providers still work. Every provider declared
by a `provides ... with ...` clause in a JDK image module is lost.** This is
not a missing module descriptor: `ModuleLayer.boot().findModule("jdk.compiler")
.get().getDescriptor().provides()` returns all four `provides` clauses, byte
for byte the same in all three arms.

It is a **boot-ordering latch**. A single call to `ModuleLayer.boot()` anywhere
in the process repairs it permanently:

```
CV --jdk-only, LatchProbe post     CV --jdk-only, LatchProbe twice (control)
  step1 JavaCompiler providers = 0   ctl call1 = 0
  step1 getSystemJavaCompiler = null ctl call2 = 0
  touch bootLayer modules=70         ctl call3 = 0
  step2 JavaCompiler providers = 1   ctl getSystemJavaCompiler = null
  step2 getSystemJavaCompiler = com.sun.tools.javac.api.JavacTool
```

The control matters: three consecutive `ServiceLoader.load` calls with no
`ModuleLayer` touch return 0 every time, so the latch is `ModuleLayer.boot()`,
not "the first call warms something up". `ModuleLayer.boot()` alone is enough —
`.modules()` and `.findModule(...)` are not required.

### 2b. Why `--real-jdk` does not show it

`CRATONVM_DIAG_SERVICELOADER=1`, same probe, same binary:

```
--real-jdk : [SL-DBG] ServiceLoader service=javax.tools.JavaCompiler
             loader_delegation=false descriptors=0 providers=1
             (["com.sun.tools.javac.api.JavacTool"])
--jdk-only : (no SL-DBG line at all)
```

Under `--real-jdk` CratonVM's own `ServiceLoader` override
(`native-builtins/src/service_loader.rs`, `discover_providers`) runs and finds
the provider by a module route (`descriptors=0`, i.e. not from any
`../../../apps/META-INF/services` file). Under `--jdk-only` that override is not registered,
the real JDK `ServiceLoader` bytecode runs instead, and it reads a module
services catalog that was never built.

Why it was never built is documented in the tree, at
`vm-cli/src/main.rs:4076-4096`:

> `initPhase2` / `initPhase3` are pure-Java methods on `java.lang.System` that
> finalise modules + classpath [...] We don't run them end-to-end in cratonvm
> (the real-JDK module graph resolution pulls in subsystems we don't
> implement) [...] INTENTIONAL (reviewed): skipping initPhase2/3 here is a
> deliberate boot-sequencing choice, **NOT a silent wrong-result stub.**

That last clause was true while the native `ServiceLoader` override covered for
it. Under `--jdk-only` the override is gone and the skip *is* a silent
wrong-result: `ServiceLoader` answers "no providers" with no warning, no
refusal line, and no `--jdk-only-report` entry. This is exactly the class of
breakage Phase 2 exists to catch — a native bridge retired by family, and a
real workload that quietly changes behaviour because the thing the bridge was
compensating for is still missing.

### 2c. The second defect, behind the first

Once H2 is on the `javacSun` path it *can* die — not always, see below — on
`NoSuchMethodError: java.lang.Object.flush()V` from
`java.io.OutputStreamWriter.flush()`. The VM logs the resolution failure
directly:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/Object.flush()V"
  caller="java/io/OutputStreamWriter.flush()V @pc=7"
```

`OutputStreamWriter.flush()` is `se.flush()` where `se` is declared
`Lsun/nio/cs/StreamEncoder;` (the layout CratonVM itself declares at
`classloading/src/class_manager.rs:12490`). Resolving that call against
`java/lang/Object` means the receiver in `se` had class id 0 — an encoder
object that was never given the real `StreamEncoder` class. `alloc_stream_encoder`
(`native-io/src/stream_encoder.rs:288-341`) has an explicit comment about this
exact failure mode and a fallback intended to prevent it.

**This one is intermittent and did not reproduce on demand.** A probe covering
all four `OutputStreamWriter` constructors, `PrintWriter` over a replaced
`System.err`, and a full `com.sun.tools.javac.Main.compile(...)` invocation is
green on HotSpot *and* on CratonVM `--jdk-only`. More tellingly, running the
failing class itself — `TestAlterSchemaRename`, `--jdk-only`, fresh cwd, with
`getSystemJavaCompiler()` confirmed `null` so the `javacSun` path *was* taken —
reached `CORPUS-END completed=true`. The javac path is therefore not
intrinsically broken.

**A receiver whose class id is 0 is the `ClassId(0)` family**, which has its own
record at
`internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-classid0-stale-address-family-FIXED.md`.
That page closed on the `"result" is null` face and says explicitly of the
other two:

> If a `ClassId(0)` receiver reappears in `TestMultiThread`, reopen this page
> rather than starting a new one, and check `object_degradation_count()` first.

This witness is a `ClassId(0)` *dispatch-miss* — the exact face that page
admits 0-in-20 did not retire — in H2, on this binary, under `--jdk-only`. It
belongs there, not in a new page. `se_key` is `identity_hash_code`
(`native-io/src/stream_encoder.rs:125`), i.e. GC-stable, so this is **not** the
address-keyed side-table family.

**Reproduction rate measured: 0 in 8.** `TestAlterSchemaRename`, `--jdk-only`,
same binary, a fresh cwd for each run:

```
REPEAT #1..#8  rc=0  objectflush_warnings=0  CORPUS-END ... completed=true
```

Eight for eight green, and `grep -c 'java/lang/Object.flush()V'` is **0** in
every log — the VM did not even *log* the resolution failure, so this is not
"failed later for another reason", it is the defect not occurring.

Against that, the corpus run hit it in **2 of 2** classes that exercise
`CREATE ALIAS` (`TestAlterSchemaRename` and `TestCases`, in the same run, minutes
apart). The distinguishing variable between the two populations is not the mode
and not the class: it is that the corpus run executed fourteen classes
back-to-back **in one shared, accumulating working directory** (§1), while every
repeat here got a clean one. Whether that is causal (heap/GC state, database
size, JIT warmth) or coincidental is **not established** — 0-in-8 bounds the
per-run rate loosely at best, and 8 runs is not enough to separate "rare" from
"needs a dirty cwd".

What this does establish: the `ClassId(0)` witness is **not** a deterministic
consequence of taking the `javacSun` path, so §2a is a divergence in behaviour
but not, on its own, the cause of the two DIVERGE verdicts.

## 3. The two CV-TIMEOUT

The driver warns that on this VM a timeout is very often a SIGSEGV that printed
no result line. Both were re-run **alone, in a fresh cwd**, with the VM's own
instrument rather than a bigger `--timeout`:

```
<cv> --java-home <jdk25> --jdk-only --stack-dump-on-timeout=180 \
     -cp <corpus classpath> CorpusMain <class>
```

### 3a. `TestCompatibility` — a HANG, not a crash. Both modes.

Zero crash markers in the log (`grep -c 'SIGSEGV|access violation|rust
panic|fatal runtime'` = **0**); the watchdog fired cleanly at its deadline and
dumped. The dump names the wait site exactly:

```
--- T19.H1 stack dump (wait-site): tid=0 name="main" frames=7 ---
tid=0 depth=3 class=org/h2/test/db/TestCompatibility method=test
tid=0 depth=4 class=org/h2/test/db/TestCompatibility method=testConcurrentAutoIncrement pc=129
tid=0 depth=5 class=java/lang/Thread method=join desc=()V
```

**77 registered threads; 50 of them are live inside
`org.h2.command.Command.executeUpdate`.** `TestCompatibility.java:485` is
`int nThreads = 50;` — the test fires 50 concurrent JDBC `INSERT`s against an
auto-increment column and joins them. HotSpot runs the entire class in 3.6 s;
CratonVM has not drained those 50 threads after 180 s.

Successive dumps of the same worker show different `pc` values inside
`Command.executeUpdate`, so threads *are* advancing — this is contention
collapse, not a hard deadlock.

This is **not** strict-mode-specific: `TestCompatibility` is CV-TIMEOUT under
`--real-jdk` too. It is a concurrency-throughput defect, and it is plausibly
the same territory as this worktree's own branch name
(`h2-testmultithread-concurrent-timeout`) and the existing
`TestMultiThread` records.

### 3b. `TestAnalyzeTableTx` — neither crash nor hang: a throughput failure, mode-dependence UNRESOLVED

Same instrument, same treatment. Crash markers: **0**. But the dump says
something quite different from §3a — only **5 registered threads**, `main` is
`blocked=false` (RUNNING bytecode), and successive dumps of `main` show it
*moving*:

```
tid=0 depth=4 java/sql/DriverManager.getConnection
tid=0 depth=5 org/h2/engine/ConnectionInfo.<init>      -> readSettingsFromURL
tid=0 depth=5 org/h2/engine/ConnectionInfo.<init>      -> convertPasswords -> SHA256.getKeyPasswordHash
tid=0 depth=5 org/h2/engine/Session.<init>
tid=0 depth=5 org/h2/engine/SessionRemote.connectEmbeddedOrServer  pc=0
tid=0 depth=5 org/h2/engine/SessionRemote.connectEmbeddedOrServer  pc=67 -> Engine.openSession
```

That is a workload making progress, not a wait site and not a fault. The
summary agrees: `tid=0 ... blocked=false`, i.e. RUNNING bytecode, with only
`Common-Cleaner`, the MVStore background writer and two idle H2 pool threads
beside it.

One signal here is worth recording even though it is not the verdict. Every
`TestAnalyzeTableTx` run under `--jdk-only` logs the VM's own CAS probe:

```
T19_H6_CAS_DIAG cas_long FAIL #0 class=org/h2/mvstore/Page$NonLeaf slot=1
   current=Long(1) expected=Long(0) new=Long(4123168616643)
T19_H6_CAS_DIAG cas_long FAIL #2 class=org/h2/mvstore/Page$Leaf slot=1
   current=Long(20066087242954) expected=Long(0) new=Long(1)
```

The values are correctly `Long`-tagged (not `Double`, not `Object(None)`), so
this is **not** the tag-loss the probe was written to catch —
`unsafe_natives_ext.rs:2274` says it exists to see "whether the heap returned a
Double-tagged bit pattern for a long slot". But the same comment notes the
rate-limit is there "to avoid flooding when a livelock fires the CAS millions
of times", and the probe's five slots are consumed within ~90 s on a workload
HotSpot finishes in 15-48 s. Worth a look by whoever owns MVStore `Page`
throughput; not evidence of a correctness fault on its own.

**And the two corpus runs are not comparable on this row.** The first run capped
each arm at `--timeout 200`; the control ran at `--timeout 420`. The control's
own elapsed column settles it:

```
org.h2.test.db.TestAnalyzeTableTx   AGREE   cv_ms=279848   hs_ms=47508
```

**280 seconds** — comfortably inside a 420 s cap and comfortably outside a
200 s one. `TestAnalyzeTableTx` did not behave differently under `--real-jdk`;
it was given 220 more seconds. The same confound touches every TIMEOUT row:

| class | jdk-only cap 200 s | real-jdk cap 420 s |
|---|---|---|
| `TestAnalyzeTableTx` | TIMEOUT (202 s) | AGREE at **280 s** |
| `TestBigResult` | AGREE at 118 s | AGREE at **273 s** |
| `TestCases` | DIVERGE at 14 s | TIMEOUT (421 s) |
| `TestCompatibility` | TIMEOUT (200 s) | TIMEOUT (426 s) |

`TestBigResult` moved 118 s → 273 s between two runs *of the same workload*,
which is the scale of drift this host produces.

**So the run was repeated at a matched budget — and the cap was not the whole
story.** `TestAnalyzeTableTx`, `--jdk-only`, alone, fresh cwd, same 420 s the
control had:

```
MATCHED org.h2.test.db.TestAnalyzeTableTx --jdk-only cap=420s rc=124 (killed)
```

It did **not** finish, where `--real-jdk` finished the same class in 280 s.
That kills the tidy "it was just the cap" reading this section originally
carried, and it is recorded here because the tidy reading was written first and
was wrong.

What is actually established for this class:

* **Not a crash** — 0 crash markers, watchdog dumped cleanly.
* **Not a hang** — `main` is `blocked=false` and observably advancing.
* **A throughput failure**, and one that *may* be mode-dependent: one
  observation of ">420 s" under `--jdk-only` against one observation of 280 s
  under `--real-jdk`, versus 15-48 s on HotSpot.

**That "may" is doing real work.** Two single observations, taken minutes apart
under different concurrent load, on a host where `TestBigResult` varied 2.3x
between two runs of the same workload in the same mode, do not separate a mode
effect from host drift. Settling it needs interleaved ABBA pairs, not one run
per arm — and this document deliberately does not claim it. What can be said
without a stopwatch: the original `CV-TIMEOUT` was still measured at a
*different* cap than the control's `AGREE`, so the two corpus runs remain
non-comparable on this row regardless of which way the underlying truth falls.

### 3c. `TestBackup` — a harness artefact, not a VM defect

`TestBackup` DIVERGEd in the control with
`MVStoreException: Chunk 2 not found`, having AGREEd in the first run. Re-run
alone, **each arm in its own fresh cwd**:

```
TestBackup --jdk-only  rc=0  CORPUS-END ... completed=true
TestBackup --real-jdk  rc=0  CORPUS-END ... completed=true
TestBackup HotSpot     rc=0  CORPUS-END ... completed=true
```

All three green. The DIVERGE was a corrupt store carried over in the shared
working directory described in §1 — the driver's missing `cd $wd`. It should
not be counted against the VM, and it is direct evidence that the workdir
defect manufactures false divergences.

## 4. Wider sample — deliberately not taken, and why

A 34-class package-stratified sample was prepared (`discover h2` yields 217
classes across 14 packages; the original 14 are simply the first alphabetically
and are 13 `org.h2.test.db` plus one `auth`, so they sample one package). It was
queued twice and cancelled twice, in favour of the work in §3.

The reason is §3b. **A wider run at a fixed per-class wall-clock cap would have
produced more rows of exactly the kind that just proved unattributable.** On
this host the same workload moved 118 s → 273 s between two runs; a 200 s cap
turns that drift into TIMEOUT verdicts, and a run that reports "N CV-BROKEN"
made of such rows is worse than no run, because it reads as a VM result. Three
of the four failures this document was asked to diagnose dissolved on contact
with a control, and two of those dissolved for measurement reasons rather than
VM reasons.

What a wider sample needs first, in this order:

1. **The workdir fix** (§1 / nomination N3) — otherwise classes contaminate each
   other's databases and manufacture divergences like `TestBackup` (§3c).
2. **A single cap used by every arm and every run**, recorded in the TSV header
   (it already is) and *checked* before two runs are compared. Better still, a
   verdict of `CV-SLOW` distinct from `CV-TIMEOUT` when the arm is provably
   still making progress — §3a and §3b are both "TIMEOUT" today and they are
   completely different findings.
3. A quiet host, or an accepted rule that TIMEOUT rows are provisional — which
   the 2026-08-05 triage already stated and which this run re-learned.

The 17- and 34-class lists are on disk and the run is one command once (1) and
(2) land.

## 5. What to fix, in priority order

**N1 — the boot module layer is never materialised, so the first
`ServiceLoader.load()` in a process silently answers "no providers".**
`vm-cli/src/main.rs:4076-4096`. Retiring the `ServiceLoader` `SyntheticStub`
under `--jdk-only` is *correct* — `java.util.ServiceLoader` is pure Java and
`native-builtins/src/service_loader.rs:3698` tags it `SyntheticStub`
deliberately. What the retirement exposed is that the real bytecode depends on
a module bootstrap the VM skips. The comment at `main.rs:4088` asserts the skip
is "NOT a silent wrong-result stub"; under `--jdk-only` it now is one. Measured
minimal repair: forcing `ModuleLayer.boot()` once during init restores every
module-declared provider (§2a). That is a behavioural patch over a boot-order
gap, not the principled fix, and should be labelled as such.

**N2 — `TestCompatibility`: 50-thread contention collapse.** §3a. Both modes.
Not strict-mode work.

**N2b — `TestAnalyzeTableTx` throughput, and settle whether it is
mode-dependent.** §3b. Needs interleaved ABBA pairs at one cap, not one run per
arm; today's evidence is >420 s strict vs 280 s real, one observation each.

**N3 — `run-corpus.sh` never applies `corpus_workdir`.** `run-corpus.sh:426`
assigns `$wd` and no line uses it. Consequences measured here: H2 databases
written into the git worktree as an untracked, un-ignored `data/`; state shared
between classes, runs and modes; and at least one false DIVERGE (§3c).

**N4 — `CV-TIMEOUT` conflates three findings.** §3a is a wait site, §3b is a
workload that needed 40% more budget, and the driver's own note says a timeout
"is very often a SIGSEGV". Neither of the two timeouts here was a crash. A
`CV-SLOW` verdict for an arm still making progress, plus a refusal to compare
runs whose `# timeout=` headers differ, would have prevented the wrong
attribution this document had to undo.

## 6. What was run and what was read

**Run** (this session, this host, binary
`/c/craton/jdkonly-wave2-target/release/cratonvm.exe`):

* the `--real-jdk` control over the same 14 classes
  (`regression-suite/corpus/out/h2-real-jdk-20260812-203313/`);
* `ToolProbe`, `SvcProbe`, `SvcMatrix`, `ProvProbe`, `LatchProbe`, `OswProbe`
  and a classpath-`../../../apps/META-INF/services` probe, each under HotSpot 25,
  `--jdk-only` and `--real-jdk`;
* `TestCompatibility` and `TestAnalyzeTableTx` alone, fresh cwd,
  `--stack-dump-on-timeout=180` (§3a, §3b);
* `TestBackup` alone, fresh cwd, all three arms (§3c);
* `TestAlterSchemaRename` under `--jdk-only` repeatedly, fresh cwd each, with
  and without a `ModuleLayer.boot()` touch (§2, §2c).

**Read**, not run: the jdk-only logs from
`regression-suite/corpus/out/h2-jdk-only-20260812-195247/`; H2's
`SourceCompiler.java` and `TestCompatibility.java`; JDK 25 `ToolProvider.java`
from `lib/src.zip`; `vm-cli/src/main.rs`,
`native-builtins/src/service_loader.rs`, `native-api/src/registry.rs`,
`native-io/src/stream_encoder.rs`, `classloading/src/class_manager.rs`,
`regression-suite/corpus/run-corpus.sh`;
`internal/jdk-only/h2-under-jdk-only-three-arm-triage-20260805.md` and
`internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-classid0-stale-address-family-FIXED.md`.

**Wall-clock is not quoted as a result anywhere here**, with one exception that
is called out as such: §3b quotes elapsed times *to show that two runs are not
comparable*, which is the opposite of a throughput claim. The host is shared and
loaded, and several probe VMs ran alongside the control arm.

**Worktree note.** This worktree was clean at the start of this session and by
its end carried modifications to `native-builtins/`, several
`docs/known-issues/jdk-only/W7-*.md`, four `regression-suite/corpus/corpora.d/*.sh`
and some `regression-suite/build/*.class` that **this lane did not make** —
another session is working in the same checkout. The only files this lane wrote
are this document and (via the driver) `regression-suite/corpus/out/`.
