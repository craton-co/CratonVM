# `TestContextAotGeneratorIntegrationTests.processAheadOfTimeWithWebTests` — QDox's `Parser.yyparse` throws under a genuine CratonVM concurrency gap, not the Aug-6 `native_unmod_get` bug

## Status

**OPEN.** Root cause narrowed to a real CratonVM concurrency defect (weaker
static-field-publication guarantees than HotSpot under concurrent first use of
a class), demonstrated with a standalone reproducer independent of Spring —
but not pinned to a specific VM source line, and the exact trigger inside the
single-threaded Spring test is not confirmed. **Confirmed NOT a regression of
the 2026-08-06 `native_unmod_get`/`al_state` bug** (see below).

## The symptom

Spring Framework, Generational GC arm, 2026-09-05 rerun (class list
`nonpassed-gen-20260905.tsv`):

```
FAILCAUSE org.springframework.test.context.aot.TestContextAotGeneratorIntegrationTests :: processAheadOfTimeWithWebTests() :: java.lang.IllegalStateException: Unable to parse source file content: <~20-line dump follows>
```

Reproduces in isolation (`found=4 succ=3 fail=1`) and in **all three** GC arms'
full-suite reruns from the same day (gen `.../out/jit-real-custom-20260905-182523`,
G1 `...-182527`, ZGC `...-182530` all show this exact FAILCAUSE) — deterministic
and collector-independent, which rules out a GC-relocation/timing confound.

## Why this needed care rather than a fresh investigation

`beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`
documents the **identical wrapper message** — `IllegalStateException: Unable to
parse source file content:` thrown by `SourceFile.getClassName()` — for a
different class, root-caused to `native_unmod_get` (`native-collections/src/lib.rs`)
bounds-checking `Collections.unmodifiableList`-wrapped QDox lists against
`al_state`'s "layout I cannot read" sentinel and turning a valid `get(0)` into
a spurious `ArrayIndexOutOfBoundsException`. The wrapper message is generic
(it fires for any exception `SourceFile.getClassName()` catches, and always
embeds the generated source), so seeing it again does not by itself mean the
Aug-6 bug regressed.

**Checked and ruled out:** `git log -- native-collections/src/lib.rs` since
2026-08-06 shows no reversion of that fix; `native_unmod_get` (line ~66337) and
`unmod_view_size` (line ~5514) still stand aside from the pre-check whenever
`al_state`'s DATA slot is `None`, exactly as the fix requires. The current
failure's real cause (below) is not an `ArrayIndexOutOfBoundsException` at all
and does not touch `native_unmod_get`.

## Getting the real cause

`run-suite.sh`'s `KRun` only prints `e.getMessage()`, which for this exception
is just the generated-source dump with no "Caused by". Two ways to see the
full chain, both used here and both agreeing:

1. `KRun.java` (found in several old worktrees, e.g.
   `/data/cvm-devcheck/apps/spring-suite-runner/KRun.java`, not present in the
   currently-deployed `KRun.class`'s source tree) honors `KRUN_STACK=1` to
   `printStackTrace` every failure. Re-running the single-class repro from the
   task with `KRUN_STACK=1 ./run-suite.sh run --list ...` puts the full chain
   in `raw.log`.
2. `KRunM.java` (`KRunM <class> <method>`, already compiled and present at
   `/data/cratonvm/apps/spring-suite-runner/KRunM.class`) always prints the
   full stack trace for a single test method. Ran directly against
   `processAheadOfTimeWithWebTests` with the same classpath/args-file the
   harness used.

Both show:

```
java.lang.IllegalStateException: Unable to parse source file content: ...
	at org.springframework.core.test.tools.SourceFile.getClassName(SourceFile.java:188)
	...
Caused by: java.lang.ClassCastException: class com.thoughtworks.qdox.parser.structs.TypeDef cannot be cast to class com.thoughtworks.qdox.parser.structs.TypeDef
	at com.thoughtworks.qdox.parser.impl.Parser.yyparse(Parser.java:3177)
	at com.thoughtworks.qdox.parser.impl.Parser.parse(Parser.java:2006)
	at com.thoughtworks.qdox.library.SourceLibrary.parse(SourceLibrary.java:232)
	at com.thoughtworks.qdox.library.SourceLibrary.addSource(SourceLibrary.java:94)
	at com.thoughtworks.qdox.library.SourceLibrary.addSource(SourceLibrary.java:89)
	at com.thoughtworks.qdox.library.SortedClassLibraryBuilder.addSource(SortedClassLibraryBuilder.java:162)
	at com.thoughtworks.qdox.JavaProjectBuilder.addSource(JavaProjectBuilder.java:174)
	at org.springframework.core.test.tools.SourceFile.getClassName(SourceFile.java:176)
```

**Disposition: (b), not (a) or (c).** Not the Aug-6 bug (different exception
class, different site, the fix is intact). Not obviously unrelated either —
it is a genuine, reproducible defect in the same functional area
(`SourceFile.getClassName()` / QDox), just a different mechanism: a
`ClassCastException` naming the **same class on both sides**
(`TypeDef` cannot be cast to `TypeDef`) is the textbook shape of two
different `Class` objects sharing one name — see the parallel investigation
in `docs/known-issues/springboot/publicsuffixlist-classloader-identity-forked-classpath-tests-20260905.md`
for that shape's other occurrence this session. **The two are NOT confirmed
to share a root cause** — see "What this is not" below.

## Isolating it: single-threaded QDox alone does not reproduce it

`org.springframework.core.test.tools.SourceFile.getClassName(String)`
(`spring-core-test/src/main/java/.../SourceFile.java:176`) creates a **fresh**
`JavaProjectBuilder` per call — no shared/cached builder across the several
generated files this test processes. A standalone probe
(`docs/known-issues/repros/qdox-parser-static-array-race/QdoxProbe2.java`)
that parses the *exact* generated source from the failure (extracted from the
message text; see note on extraction below), single-threaded, single call:
**succeeds on both HotSpot and CratonVM.** So does a tight single-thread loop
of 5000 repeated fresh-builder parses of the same content on CratonVM (rules
out a simple JIT-warmup/tiering trigger). Nothing about the content itself,
or one-shot vs. many-shot single-threaded execution, reproduces the bug.

*(Extraction note: the raw exception message is printed twice in the
`KRUN_STACK`/`KRunM` output — once by the harness's own `FAILCAUSE` line via
`getMessage()`, once again by `printStackTrace()` — so a naive line-range grab
concatenates two copies into one file and produces a bogus javac syntax error
at the seam. The real single copy is one clean, brace-balanced ~517-line file;
see `sample-generated-source.txt` in the repro dir.)*

## What does reproduce it: concurrent first use of QDox's `Parser` class

`com.thoughtworks.qdox.parser.impl.Parser` (disassembled via `javap`, QDox
2.2.0) carries two **non-final static fields**:

```
static short[] yytable;
static short[] yycheck;
```

lazily decoded on first use by static methods of the same name — a classic
unsynchronized lazy-singleton pattern (no `volatile`, no lock). This is a
data race by the JLS/JMM's own rules on any JVM. Storming that first-use
window from several threads
(`docs/known-issues/repros/qdox-parser-static-array-race/QdoxConcProbe.java`,
N threads each running fresh-builder parses of the same content) reproduces
failures **on CratonVM reliably and quickly**:

| VM | config | result |
|---|---|---|
| CratonVM (Generational, gen wrapper) | 4 threads x 1500 iters (6000 total) | **7 failures**, all `ArrayIndexOutOfBoundsException: Index N out of bounds for length 0` in `SourceLibrary.addSource(SourceLibrary.java:94)`, first one at iteration 201 |
| real HotSpot JDK 25 | 8 threads x 3000 iters, **3 separate runs** (72000 total) | **0 failures** |

"...for length 0" is exactly the signature of one thread observing the
static array field as its pre-decode (empty) state — an unsafe-publication
symptom.

**This does not by itself prove QDox is safe on HotSpot** — the underlying
pattern is a genuine data race in a third-party library, not something either
JVM is contractually obligated to make safe. What it does show is a **measured
gap**: CratonVM exposes this race readily (7 failures in 6000 concurrent
parses across a few hundred iterations), where 72000 concurrent parses on
HotSpot exposed nothing. That is evidence of a real difference in how
reliably each VM publishes a freshly-written static array reference to other
threads racing to read it — weaker in CratonVM — even though it is not
formally a "HotSpot is correct, CratonVM is buggy" comparison, since the
racing code itself is not correctly synchronized on either VM.

## What this is not (yet) confirmed to be

- **Not confirmed to be the actual trigger inside the real (single-threaded)
  Spring test.** `grep`ing `spring-core-test`'s `SourceFile`/`TestCompiler`/
  `DynamicClassLoader`/etc. for `parallelStream`/`Executor`/extra `Thread` use
  turns up none — Spring's own call path to `getClassName()` for this test
  appears to be single-threaded application code. The concurrent probe
  demonstrates the underlying static-field race exists and that CratonVM is
  measurably more exposed to it than HotSpot; it does not demonstrate that
  *this specific* single-threaded test hit it via application-level threads.
  The live candidate is CratonVM's own background JIT compiler thread racing
  the interpreter thread over the same class's metadata/static fields during
  first use — plausible (a JIT compiler thread is active on every run,
  unlike the synthetic probe's explicit application threads) but **not
  traced to a specific VM source line**.
- **Not confirmed to share a root cause with the `PublicSuffixList`
  classloader-identity bug** (`docs/known-issues/springboot/publicsuffixlist-classloader-identity-forked-classpath-tests-20260905.md`).
  That bug requires Spring's forked/child-classloader test infrastructure
  (`@CompileWithForkedClassLoader` / `ClassPathExclusions`); this test uses
  neither. Both produce a "class X cannot be cast to class X" (or, here, an
  `ArrayIndexOutOfBoundsException` on a stale-length array) shape, which is
  suggestive of a broader family (something about CratonVM's handling of
  concurrent/repeated class initialization or static-state publication), but
  that is a hypothesis connecting the two, not a demonstrated shared cause.

## No fix attempted

Per this session's brief: document honestly rather than guess. A real fix
here means auditing CratonVM's class-initialization/static-field-publication
memory-model guarantees under concurrency (interpreter-vs-JIT-thread and/or
app-thread-vs-app-thread) — materially bigger than this triage pass, and not
something to patch speculatively against a single third-party symptom.

## Reproduce

```bash
# Single-class repro (from the task), with full stack trace:
cd apps/spring-suite-runner
echo -e "apps/spring-framework/spring-test\torg.springframework.test.context.aot.TestContextAotGeneratorIntegrationTests" > /tmp/single-class.tsv
CRATONVM_BIN=<gen-wrapper> JDK25=<jdk25> OUTROOT=/tmp/single-out KRUN_STACK=1 ./run-suite.sh run --list /tmp/single-class.tsv
# grep raw.log for "Caused by: java.lang.ClassCastException"

# Standalone, VM-only, no Spring/Gradle:
javac -cp qdox-2.2.0.jar docs/known-issues/repros/qdox-parser-static-array-race/QdoxConcProbe.java -d /tmp
<cratonvm> --java-home <jdk25> -cp /tmp:qdox-2.2.0.jar QdoxConcProbe \
  docs/known-issues/repros/qdox-parser-static-array-race/sample-generated-source.txt 4 1500
```
