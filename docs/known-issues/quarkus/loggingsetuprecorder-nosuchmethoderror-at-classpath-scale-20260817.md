# `LoggingSetupRecorder.initializeLogging` `NoSuchMethodError` — blocks 100% of quarkus tests, only at real classpath scale

**Status: OPEN, not yet root-caused to a fix.** Found 2026-08-17, commit
`3ef3eb744`/`7416e3db0` (Azure host, ZGC binary). This is the defect behind
what a prior session reported as "quarkus 3,939/3,939 PASS (100%)" — that
number was vacuous (see the sibling harness-fix note below): every one of
those 3,939 classes actually recorded `started=0`, i.e. zero JUnit test
methods ever ran. This doc is the root-cause investigation for *why*.

## What it is

Every quarkus test class, on every collector, dies identically before any
`@Test` method starts:

```
NoSuchMethodError method="io/quarkus/runtime/logging/LoggingSetupRecorder.initializeLogging(Lio/quarkus/runtime/logging/DiscoveredLogComponents;Ljava/util/Map;ZLio/quarkus/runtime/RuntimeValue;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Ljava/util/List;Lio/quarkus/runtime/RuntimeValue;Lio/quarkus/runtime/LaunchMode;Z)Lio/quarkus/runtime/shutdown/ShutdownListener;" caller="io/quarkus/test/config/LoggingSetupExtension.<init>()V @pc=7"
```

`LoggingSetupExtension` is a global JUnit5 `Extension` every quarkus test
class registers; its constructor calls
`LoggingSetupRecorder.handleFailedStart()` to get basic logging up before the
first test runs. That call chain (`handleFailedStart()` →
`handleFailedStart(RuntimeValue)` → `new LoggingSetupRecorder(...)` →
`initializeLogging(...)`) is what fails, on the FIRST class of every single
forked VM — meaning this reproduces at the very start of every run, before
any test-specific code executes at all.

## The method is not missing, and the signature is not actually wrong

Before assuming a registration/signature gap (the initial hypothesis), this
was checked three ways, all agreeing:

1. **`javap` on the real jar** (`quarkus-core-999-SNAPSHOT.jar`) shows exactly
   one `initializeLogging` overload, with **14 parameters** (7 of them
   consecutive `List<...>` — this shape matters, see below), returning
   `ShutdownListener`.
2. **The module's compiled `target/classes` copy** (the one actually first on
   the real classpath) is **byte-identical** (`cmp` — same 50065-byte file)
   to the jar copy. No version skew between what the caller was compiled
   against and what's on disk.
3. **Disassembling the caller** (`handleFailedStart(RuntimeValue)`'s
   `invokevirtual` at bytecode offset 167) shows it was compiled against the
   exact same 14-parameter descriptor.

But the descriptor CratonVM reports in the `NoSuchMethodError` has only
**13 parameters** — one `Ljava/util/List;` short (verified precisely:
`grep -c` on the raw error text finds 6 `Ljava/util/List;` occurrences where
the real method/caller both have 7). **CratonVM's own report of "the method
it looked for" is malformed**, not the class file.

## The parser code looks correct on inspection

Read in full and found structurally sound (proper JVMS-compliant loop, no
special-casing that could merge/dedupe adjacent identical reference-type
segments):

- `classloading/src/vtype.rs` — `param_types_from_descriptor` /
  `field_descriptor_len` (used by the verifier's `VType` machinery).
- `reader/src/method_descriptor.rs` — `MethodDescriptor::parse`.
- `reader/src/field_type.rs` — `FieldType::parse_partial_depth`, including the
  `b'L'` arm that finds the terminating `;`.

None of these obviously explain a dropped parameter, and hand-written repros
against them (see below) don't reproduce the bug — so either the corruption
happens somewhere else entirely, or it's a much subtler interaction these
three files don't individually show.

## It does not reproduce in isolation — only at real classpath scale

Three escalating repros, run against the actual CratonVM release binary:

1. **A static method with 7 consecutive `List<Object>` params**, called
   directly and via reflection. **Passes.**
2. **An instance method with the same shape**, invoked via `invokevirtual` on
   a freshly constructed object, inside a constructor-chain matching the real
   call shape (`static()` → `static(arg)` → `new X(...)` →
   `x.method(...)`). **Passes.**
3. **The real method itself**, called via a one-line driver
   (`repro-lsr-nsme/MiniDriver.java`: `LoggingSetupRecorder.handleFailedStart()`)
   run with the harness's own `common.args` classpath (**4,230 entries**,
   ~420KB argfile) instead of the full JUnit/quarkus-test machinery. **Fails,
   identically, in ~1 minute** — a much faster repro loop than booting a full
   quarkus test class (~2 minutes) for iterating on this bug.

So the trigger is not "a method with many repeated List params" in the
abstract — it needs the real classpath. The likely reason: `handleFailedStart
(RuntimeValue)` builds a `SmallRyeConfig` via `SmallRyeConfigBuilder.build()`
before ever reaching the `initializeLogging` call, and that build does a
**wide `ServiceLoader`/`../../../apps/META-INF/services` scan across the whole classpath**
for `ConfigBuilderCustomizer` and `ConfigSource` providers — confirmed by
partial classpath subsetting: shrinking the classpath to ~245 entries still
pulled in providers from `smallrye-reactive-messaging`
(`ReactiveMessagingConfigBuilderCustomizer`) and `resteasy-microprofile-config`
(`ResteasyConfigSource`/`FilterConfigSource`) that are not obviously related
to logging at all, each requiring its own further transitive jars just to
class-load (a genuine trap for future subsetting attempts — the customizer
scan does not appear to short-circuit on failure, so any excluded provider's
missing dependency surfaces as a *different*, unrelated `NoClassDefFoundError`
rather than cleanly skipping).

**Not yet confirmed**: that classpath *entry count* itself (as opposed to
*ServiceLoader-discovered provider count*, or something else scale-related)
is the actual variable. Time ran out on this investigation before a clean
apples-to-apples "same functional classpath, only entry count differs"
comparison was completed — the subsetting attempts kept surfacing new missing
transitive dependencies (`jakarta.annotation.Priority`,
`jakarta.servlet.FilterConfig`, ...) rather than a clean pass/fail signal.

## Candidates surveyed and not (yet) implicated

A broad `grep` for every `*_CACHE_CAP`/FIFO-bounded cache in the codebase
(there are dozens, introduced across several "round N audit fix" security
hardening passes) turned up a few tempting near-matches that on inspection
don't fit:

- `classloading::class_path::CANONICALIZE_CACHE_CAP` (1024) — bounds
  `fs::canonicalize` memoization. Eviction just means a future lookup redoes
  the syscall; can't return a wrong answer.
- `vm::native::jni::DESCRIPTOR_CACHE_CAPACITY` (1024) — parses **JNI native**
  call descriptors into arg-marshalling tags; keyed by exact descriptor
  string (`HashMap<String,_>`, no false-positive collisions), and
  `initializeLogging` isn't a native method anyway.
- `jit::helpers::VIRTUAL_TARGET_CACHE_CAP` (4096, suspiciously close to the
  4,230-entry classpath) — a JIT dispatch-target-by-name memo, but it's
  **clear-on-full** (not single-entry LRU eviction), and the comment/design
  explicitly notes a miss just reverts to the slower, correct locked path —
  doesn't look able to fabricate a wrong answer.

None of these were disproven with certainty (no debug build was instrumented
to watch them live) — they were ruled out by code reading, which is weaker
than a measurement. A real answer likely needs either instrumented tracing at
the exact resolution call site (`vm_exec.rs`, the `invoke_on_class_shared_no_
retarget` function around the `tracing::warn!(..., "NoSuchMethodError")` call)
showing what `descriptor` looked like at each step back to the constant pool
read, or a `rr`/similar record-replay session.

## Fast repro

```bash
cd apps/quarkus-suite-runner
cp repro-lsr-nsme/MiniDriver.java .
javac @common.args -d . MiniDriver.java   # note: common.args classpath entries
                                            # exceed argv limits — use @argfile,
                                            # not a literal -cp on the command line
CV_BIN=$(pwd)/../../target-zgc/release/cratonvm-quarkus-zgc
"$CV_BIN" -XX:+UseZGC @common.args MiniDriver
```

Oracle: `java @common.args MiniDriver` — passes in ~1s, `MINI_DRIVER_OK`.

## Related

- The harness's own PASS/FAIL classifier bug (`found>0, started=0, failed=0`
  read as `PASS`) is fixed separately in `run-quarkus-suite.sh` (adds a
  `NOSTART` status, 2026-08-17) — that fix is necessary but not sufficient;
  it makes the symptom visible, it doesn't fix the underlying
  `NoSuchMethodError`. A full rerun with the classifier fix was started, then
  killed after ~32 minutes / 26 sampled classes showed 100% `NOSTART`/`HANG`
  with zero real PASS/FAIL data — not worth the ~5-day full-suite runtime
  until this bug is fixed.
