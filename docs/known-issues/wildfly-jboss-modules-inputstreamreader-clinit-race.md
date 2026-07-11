# WildFly domain boot: `InputStreamReader(InputStream, Charset)` real-bytecode resolution now fails ~always during `org/jboss/modules/Main.<clinit>` — NEW blocking finding, gates the whole domain-boot investigation

**Status: OPEN, blocking. Not fixed. Not root-caused. This is now the front-line**
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
site (`vm/src/vm/vm_exec.rs`, search for the exact log text) to capture
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
