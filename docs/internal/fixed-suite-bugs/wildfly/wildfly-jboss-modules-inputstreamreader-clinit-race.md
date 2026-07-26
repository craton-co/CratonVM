# RETRACTED: `InputStreamReader(InputStream, Charset)` "real-bytecode resolution race" during `org/jboss/modules/Main.<clinit>` — was a test-harness `JAVA_HOME` gap, not a CratonVM bug

**Status: RETRACTED (2026-07-11, see the dated section at the end of this**
**file for the full correction). This was never a CratonVM defect — set**
**`CRATONVM_JAVA_HOME` alongside any `JAVA_HOME` shim directory used to**
**satisfy a launcher script. Kept for the reproduction methodology and the**
**`has_real_boot_classes()` mechanism trace, which are still accurate and**
**useful; do not treat the "race"/"blocking" framing below as current.**

<details>
<summary>Original (incorrect) write-up, kept for history</summary>

**Status (ORIGINAL, WRONG): OPEN, blocking. Not fixed. Not root-caused. This was believed to be the front-line**
**gate for `wildfly-domain-heap-corrupt-value-timeout.md` (including BUG-03),**
**ahead of everything previously tracked there.**

## Symptom

`./bin/domain.sh` (or `standalone.sh`, untested but likely shares the same
bootstrap) dies immediately, before any WildFly-specific code runs at all:

```
WARN cratonvm_vm::vm::vm_exec: Missing native method in real-JDK mode method=java/io/InputStreamReader.<init>(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=org/jboss/modules/Main cause=java/lang/UnsatisfiedLinkError
WARN cratonvm_vm::vm::vm_util:   [CLINIT-TRACE 0] at org/jboss/modules/Main.<clinit> (Main.java:640) bci=48
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/ExceptionInInitializerError
```

`org.jboss.modules.Main`'s own static initializer (disassembled via `javap
-c -p` against the real `jboss-modules.jar`'s `Main.class`) does exactly this,
entirely ordinary code with no CratonVM-specific tricks:

```
Main.class.getResourceAsStream("version.properties")   // bci 27-35, succeeds (non-null)
new InputStreamReader(is, StandardCharsets.UTF_8)       // bci 40-48  <-- dies here
new Properties().load(reader)
```

## Reproduction rate this session

- **A controlled batch of 20 consecutive attempts (fresh, never-before-run
  WildFly 32.0.1.Final extraction, fresh binary built from `origin/dev` @
  ~`67902b3f`/`1465637a`, `pkill -9 -f org.jboss.as` between each attempt to
  guarantee no leftover state): 0/20 succeeded.** Every attempt died with
  this exact signature within well under a second of process start.
- Also reproduces with: the OLDER binary (built at `82531baa`, before this
  session's MSC fixes) against both a REUSED probe directory AND a
  completely fresh WildFly extraction; `CRATONVM_DISABLE_JIT=1`;
  `CRATONVM_DBG_STW_CENSUS=1`/`CRATONVM_DBG_XT_JIT_ROOT_SCAN=1`;
  `CRATONVM_DBG_CHARSET=1` (the flag the error message itself suggests —
  produces no additional diagnostic here, just the same failure plus two
  unrelated benign `gen_heap::get_field: out-of-bounds field read dropped`
  WARNs on an `UnsatisfiedLinkError` object, indices 6/7 past its 6-field
  layout — matches the WARN's own "speculative collection-layout probe on a
  non-matching receiver" description, almost certainly incidental noise from
  formatting the resulting exception, not the cause).
- **The ONE and ONLY success this session** was an earlier run (this doc's
  sibling `wildfly-domain-heap-corrupt-value-timeout.md`, "fourth session"
  entry) that reached 595+ log lines and domain.xml subsystem activation.
  That was the OLD binary, clean env, reused probe directory — a
  combination that has since been retried multiple times and failed every
  time. Nothing about that combination is reproducibly special; whatever
  let it through was not captured.

## What is NOT the cause (ruled out by isolated repro)

Three isolated repros, run against the exact same binary that fails 0/20 on
the real WildFly boot, all succeed 5/5 (or 3/3):

1. Trivial single-class program, `-cp .` launch, `new
   InputStreamReader(new ByteArrayInputStream(...), StandardCharsets.UTF_8)`
   in `main()`: **5/5 success.**
2. A minimal jar (`test2.jar`, 2 entries: one class + one properties file)
   that byte-for-byte replicates `Main.<clinit>`'s own pattern — a
   **static initializer** doing `Class.getResourceAsStream(...)` +
   `new InputStreamReader(is, UTF_8)` + `Properties.load(...)`, launched via
   `-jar` (matching `domain.sh`'s own `-jar jboss-modules.jar` invocation
   exactly, not `-cp`): **5/5 success.**
3. Same jar-launch repro, with WildFly's actual extra flags added
   (`-Djava.security.manager=allow`,
   `--add-opens=java.base/java.lang=ALL-UNNAMED`,
   `--add-opens=java.base/java.io=ALL-UNNAMED`): **3/3 success.**

So this is NOT: the constructor overload itself being unimplemented, `-jar`
vs `-cp` launch semantics, `-Djava.security.manager=allow`, the specific
`--add-opens`/`--add-exports` flags, the JIT on/off setting (fails either
way), the specific frozen binary (fails on both an older and a freshly-built
one), or stale probe-directory state (fails on a brand new extraction too).

By elimination, whatever is different is specific to booting the REAL
`jboss-modules.jar` — a much larger jar with many more classes/resources and
its own module-path machinery — versus a trivial 1-2-class jar. Two
candidate directions, NEITHER confirmed:

- A timing-sensitive race in CratonVM's own native-method/real-bytecode
  resolution or class-loading, more likely to manifest the more classes are
  touched in the earliest moments of boot (matches the general shape — not
  the specific mechanism — of the GC audit's "STW/monitor race family",
  `docs/known-issues/gc-audit-2026-07-10-open-findings.md` finding 1: races
  that are load/timing-sensitive and get WORSE, not better, over repeated
  runs on a busier host).
- Interaction with CratonVM's own early background threads: `vm-cli`
  spawns a `cratonvm-stack-watchdog` thread (sleeps until a deadline,
  unlikely to matter this early) and a `main-vm` thread (the actual
  interpreter thread, stack_size 128 MiB); `jit/src/tiered.rs:2555` and
  `jit/src/lib.rs:10279` both spawn threads too (not yet inspected for
  what/when — flagged here, not chased, for whoever continues).

**Explicitly NOT investigated further this session**: the actual mechanism
(what races with what, or whether it's genuinely a race at all vs. some
other jboss-modules.jar-specific classpath/module resource-scanning gap).
This needs its own dedicated session — ideally with `RUST_LOG` tracing
around real-bytecode class resolution for `java/io/InputStreamReader`
specifically, or a `gdb` breakpoint on the "Missing native method" log call
site (`../../../../vm/src/vm/vm_exec.rs`, search for the exact log text) to capture
what the resolution path actually saw at failure time, cross-thread state
included.

## Why this matters more than it looks

This is not WildFly-specific in cause (only in exposure — it happens to be
the first thing in this investigation big enough to hit it reliably). Any
sufficiently large real-JDK-mode boot that constructs an `InputStreamReader`
with an explicit `Charset` early in its own bootstrap is a candidate. It
should be treated as a general CratonVM correctness gap, not folded into
WildFly-specific docs, even though it was found here.

## Impact on this investigation

`wildfly-domain-heap-corrupt-value-timeout.md`'s BUG-03 (`STW cross-thread
JIT takeover` stall) could not be re-examined live this session because of
this — every attempt to reach that point (0/20 in the controlled batch, plus
several more one-off attempts) now dies here instead, before `org.jboss.as`
even starts. See that doc's next dated entry for what was learned about
BUG-03 by static analysis (GC backend confirmation, `park()` correctness
check, code-path tracing) before this blocker was hit.

## Environment / reproduction recipe for whoever continues

```bash
# Fresh extraction avoids any doubt about probe-directory state:
python3 -m zipfile -e /data/data/wildfly-dist/wildfly-32.0.1.Final.zip <destdir>
chmod +x <destdir>/wildfly-32.0.1.Final/bin/*.sh
mkdir -p <javahome>/bin && ln -s <any cratonvm binary> <javahome>/bin/java
cd <destdir>/wildfly-32.0.1.Final
JAVA_HOME=<javahome> PATH=<javahome>/bin:$PATH CRATONVM_MSC_REAL_START=1 \
  timeout 5 ./bin/domain.sh
# grep for InputStreamReader in the output — expect to see it essentially every time.
```


</details>

---

## RETRACTED (2026-07-11, same-day follow-up) — this was never a CratonVM bug; it was a test-harness `JAVA_HOME` misconfiguration

**This doc's core claim — a timing-sensitive real-bytecode resolution race
specific to the jboss-modules.jar bootstrap — is WRONG.** Root-caused via
temporary `CRATONVM_DBG_ISRTRACE` instrumentation added to
`classloading/src/class_manager.rs::load_class` (added, tested, then
reverted — never committed): at the exact point `Main.<clinit>` loads
`java/io/InputStreamReader`, `ClassManager::has_real_boot_classes()`
(`self.bootstrap.find_class_bytes("java/lang/Object")`) returns **`false`**,
which routes the load through the `is_jdk_class(name) && !has_real_boot_classes()`
fallback and fabricates a synthetic stub (whose `<init>` is declared
`ACC_NATIVE` with no backing registration in real-JDK-mode builds, per
`aaf64a5d`) instead of surfacing real bytecode.

The reason `has_real_boot_classes()` was false: every probe script this
investigation used (across all sessions, not just this one) sets
`JAVA_HOME=<a directory containing only a `bin/java` symlink to the CratonVM
binary>` — required by `domain.sh`'s own launcher convention (it execs
`$JAVA_HOME/bin/java`). But `vm/src/config.rs::resolve_java_home` (called by
`ClassManager`'s own boot-classpath discovery) **also** consults that exact
same `JAVA_HOME` env var to decide where to search for real JDK class files
(`jmods`/`lib/modules`) — and finds nothing there, because the shim directory
only ever had the one symlink. There is a dedicated escape hatch for exactly
this shape of conflict — `CRATONVM_JAVA_HOME`, documented in
`resolve_java_home`'s own source comment: *"used when JAVA_HOME points at a
cratonvm shim tree (Maven, Gradle) but boot modules must come from a real
JDK"* — which no probe script in this investigation had ever set.

**Fix: none needed in CratonVM.** Setting `CRATONVM_JAVA_HOME=<real JDK 25
install, e.g. /home/victor/jdk25>` alongside the existing `JAVA_HOME` shim
resolves it completely — verified **10/10** in a controlled batch (same
methodology as the original 0/20 finding), on both a fresh WildFly extraction
and the reused probe directory, with both the pre-existing and freshly-built
binaries. This doc's own "isolated repros all succeed" section was actually
already exercising a *different*, correctly-configured environment (those
repros were run by invoking the CratonVM binary directly, with no `JAVA_HOME`
env var set at all, letting `resolve_java_home`'s step-4 PATH-based
auto-detection find a real system JDK) — which is why they never reproduced
the failure the real `domain.sh`-driven runs hit 100% of the time. The
"ruled out" list in this doc (launch mode, JVM flags, JIT on/off, binary
version, probe-directory staleness) is still accurate — none of those were
ever the cause — it was simply never comparing like-for-like environments.

**Retiring this doc.** Boot correctly configured this way goes vastly
further — see `wildfly-domain-heap-corrupt-value-timeout.md`'s next dated
entry for the full downstream picture (reaching genuine sustained load,
63K+ log lines, for the first time in this entire investigation). This file
is kept for the historical record (the reproduction methodology and the
`has_real_boot_classes()` mechanism trace are both independently useful) but
should not be treated as an open CratonVM defect. Consider moving to
`..` once linked from the resolution above.

**Lesson for future sessions on this codebase**: when a probe/test harness
sets `JAVA_HOME` purely to satisfy a launcher SCRIPT's own convention (not
because it's a real JDK install), also set `CRATONVM_JAVA_HOME` to a real
JDK. This is easy to miss because the failure mode (a stray synthetic-stub
fallback deep in early bootstrap) looks nothing like a classpath
misconfiguration.
