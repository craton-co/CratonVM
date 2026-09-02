# `java.util.logging.Logger` cannot be cast to `org.jboss.logmanager.Logger` — FIXED 2026-08-17, residual closed 2026-09-01

Retired from `docs/known-issues/quarkus/`. The cast itself was fixed on
2026-08-17 (`04483c5ee`, ancestor of `dev`) and is re-verified below against
the `dev` tip of 2026-09-01 (`56d6c3722`). What kept the page open was its
last section, and that section was wrong twice over — once about where the
next blocker was, and once about what the fix had left behind. Both are
resolved here.

## The original report, and its fix

`io.quarkus.test.AbstractQuarkusExtensionTest:112` runs, from its own
`<clinit>`:

```java
System.setProperty("java.util.logging.manager", "org.jboss.logmanager.LogManager");
rootLogger = (org.jboss.logmanager.Logger) LogManager.getLogManager().getLogger("");
```

`CLS_JBOSS_LOG_MANAGER`'s `getLogger` was registered to the same
`native_get_logger` as the plain JUL manager, which unconditionally allocates
a `java/util/logging/Logger`-shaped object, so the cast threw. The correct
allocator (`get_or_create_jboss_logger`) already existed behind
`LogContext.getLogger`; the fix pointed the manager's `getLogger` at it.
That one defect was ~78% of a full-suite rerun's failures (99/105 `NOSTART`
classes failing identically).

Two corrections to the page's own text:

* it named `native-builtins/src/jboss_logmanager.rs` as one of the two files
  that "reimplement both natively". **That file is not compiled.** No
  `mod jboss_logmanager;` declaration exists anywhere in the tree, so its 503
  lines — including a second `setLevel`/`getLevel`/`isLoggable` registration
  block for this very class, and a `register_jboss_logmanager_natives` nothing
  calls — are orphaned source. Everything live for `org.jboss.logmanager` is
  in `logmanager.rs`. (Left in place, not deleted: choosing between wiring it
  up and removing it is a separate call. What matters here is that reading it
  tells you nothing about the running VM.)
* its "What this fix exposed next" section reported a hang at
  `QuarkusTestProfileAwareClassOrderer`. That was refuted the same day by
  `quarkustestprofileawareclassorderer-not-a-hang-throughput-gap-20260817.md`
  (still open, still public): a stack dump showed the main thread *running* in
  `TestResourceManager.start` → ShrinkWrap package scanning, ~3-4x slower than
  HotSpot but finite; the orderer line merely prints just before it. That page
  owns the throughput question and is unaffected by anything here.

## The real residual: the fix handed callers a shape whose face was stubs

Before the fix, `LogManager.getLogManager().getLogger("")` returned a
JUL-shaped Logger, so everything a caller did with it next landed on the
`java/util/logging/Logger` natives — which are real: a GC-safe identity-keyed
handler list, a name-keyed explicit-level table with a nearest-ancestor walk,
and a publish path that fans a record out to ancestors' handlers.

After the fix it returns an `org/jboss/logmanager/Logger`, and *that* class's
natives were a stub set written for one job — keeping WildFly and Keycloak boot
logging from NPE-ing on the synthetic Logger's null `loggerNode`:

| method | what it did |
|---|---|
| `getHandlers()` | a fresh EMPTY array, every call |
| `addHandler` / `removeHandler` / `setHandlers` | no-op |
| `getLevel()` | constant `null` |
| `setLevel(Level)` | no-op |
| `isLoggable(Level)` | constant `true` |
| `getEffectiveLevel()` | constant `800` |
| `getParent()` | constant `null` |

Not-NPE-ing is all any of those had ever been asked for. The fix promoted the
class from "a thing WildFly logs through" to "the object ordinary application
code is handed", and the first thing
`AbstractQuarkusExtensionTest.beforeAll` does with it is:

```java
originalHandlers = rootLogger.getHandlers();     // -> always []
rootLogger.addHandler(inMemoryLogHandler);       // -> no-op
...
rootLogger.setHandlers(originalHandlers);        // -> no-op   (afterAll)
```

so its `InMemoryLogHandler` collected nothing, forever, with no error anywhere
— and `overrideLoggerLevel` (which backs `traceCategories(...)`) stashed a
`null`, set a level nothing recorded, and restored a value nothing read.

A stub answers the shape of the question. That is why it survives a green run:
no caller can tell an empty array from an empty array.

## Measured: the probe, and the three columns

`apps/quarkus-suite-runner/probes/JbossLogManagerProbe.java` (untracked, with
the rest of `apps/`) exercises every documented route to a Logger under
`-Djava.util.logging.manager=org.jboss.logmanager.LogManager` and reports a
VALUE for each, never "did not throw". Run on Temurin 25.0.3+9 and on CratonVM
with the same classpath and the same flag.

| probe line | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| `root.cast` | `CAST_OK` | `CAST_OK` | `CAST_OK` |
| `root.handlers.deltaAfterAdd` | 1 | **0** | 1 |
| `root.handlers.containsAdded` | true | **false** | true |
| `root.capture.count` | 2 | **0** | 2 |
| `root.capture.hasChildMessage` | true | **false** | true |
| `root.capture.hasRootMessage` | true | **false** | true |
| `root.handlers.containsRemoved` | false | false | false |
| `root.handlers.deltaAfterSet` | 0 | 0 | 0 |
| `level.afterSetTrace` | TRACE | **null** | TRACE |
| `level.rereadViaStatic` | TRACE | **null** | TRACE |
| `level.effectiveAfterTrace` | 400 | **800** | 400 |
| `level.freshControl.isLoggableTrace` | true | **false** | true |
| `level.freshControl.effective` | -2147483648 | **800** | -2147483648 |
| `root.effective` | -2147483648 | **800** | -2147483648 |
| `tree.kid.effective` | 900 | **800** | 900 |
| `tree.kid.isLoggableInfo` | false | **true** | false |
| `tree.kid.parentName` | `[probe.tree]` | **`<null>`** | `[probe.tree]` |
| `loggerNames.sawProbeA` | true | **false** | true |

`root.capture.*` is the line that matters most: a record logged on the CHILD
logger `probe.capture` must reach a handler installed on the ROOT. That is
exactly how the quarkus extension collects build-time log output, and it is
the assertion a "did it throw" probe can never make.

## What was actually wrong — six causes

**1. The handler face was stubs.** `addHandler`/`removeHandler` now point at
the `java/util/logging/Logger` natives (the JBoss synthetic Logger is allocated
at the same width and field map as the JUL one, so they read it unchanged);
`setHandlers` is a new replace-the-whole-set native; `getHandlers` builds the
array from the identity-keyed side list instead of minting an empty one.

**2. `removeHandler` only ever cleared one of two tables.** `addHandler` writes
both the name-keyed compatibility map and the identity-keyed `ArrayList` that
every publish actually reads; `removeHandler` cleared only the first. So
`addHandler(h); removeHandler(h)` left `h` receiving records for the rest of
the process — and that removal is what stops one test class's handler from
collecting the next class's output. This half was a JUL-side defect all along;
the JBoss face simply never reached it.

**3. The ancestor walk was shape-blind.** `resolve_jul_handler_list` walked
dotted-name ancestors with `get_or_create_logger` — always the JUL shape. Under
the JBoss manager every factory mints the *other* shape, so for the root name
`""` the walk demand-created a brand-new JUL logger and read its empty side
list while the handler sat on the JBoss root object.
`existing_loggers_for_name` now reports both shapes and creates neither: a name
nobody has asked for cannot have handlers, so the demand-creation was paying an
allocation for a guaranteed miss.

**4. The level face was stubs.** `setLevel` now funnels through
`record_jul_logger_level` (the name-keyed table every `setLevel` in the tree is
required to use) and stamps the slot; `getLevel` reads it back;
`getEffectiveLevel` and `isLoggable` consult the nearest-ancestor walk.

> **CORRECTED 2026-09-01, later the same day.** This section originally went on
> to say that `isLoggable` must not share the JUL native because "an
> unconfigured jboss-logmanager node sits at
> `effectiveMinLevel = Integer.MIN_VALUE` and logs everything, where an
> unconfigured JUL logger stops at INFO". **That is false.** The
> `Integer.MIN_VALUE` was measured with `quarkus-bootstrap-runner` on the
> classpath and is quarkus's own configuration —
> `InitialConfigurator.getInitialLevel("")` returns `Level.ALL` for the ROOT,
> which every logger then inherits. With no `LogContextInitializer` provider on
> the classpath the same probe reads `800` on both faces. The two faces DO need
> separate `isLoggable` implementations, but for two different reasons found
> later: `isLoggable(Level.OFF)` is false by NAME rather than by threshold, and
> `getMinimumLevel` is a second, independent floor. See
> `jboss-logcontextinitializer-spi-not-consulted-20260901-FIXED.md` for the
> control run and the corrected constant.

**5. `getParent` was a constant null**, so every logger looked like a root. It
is the immediate dotted-name predecessor — jboss-logmanager materialises the
whole `LoggerNode` path, so `probe.fresh` reports parent `probe` even though
nobody ever asked for `probe` — and null only for the root, which is what makes
a caller's walk finite. The old comment claimed returning a parent "would
loop"; it does not, because the root still answers null.

**6. `getLoggerNames()` read only the JUL registry**, so under the JBoss
manager it enumerated the loggers nobody had created and omitted every one that
existed.

## And one cause that was not on this class at all

CratonVM was importing `.level=INFO` from `$java.home/conf/logging.properties`
under the JBoss manager.

(This section originally attributed `root.effective` reading 800 against
HotSpot's `Integer.MIN_VALUE` to that import. Only half of that is right: the
import was real and is fixed below, but `Integer.MIN_VALUE` is not
jboss-logmanager's default — see the correction in cause 4. The import still
had to go: an explicit INFO level on the ROOT is a process-wide floor CratonVM
was inventing.)

That file is the **JDK** `LogManager`'s implicit fallback. When
`java.util.logging.manager` names the JBoss subclass, the primordial read runs
*that class's override*, whose `doConfigure` resolves a `ConfiguratorFactory`
through `ServiceLoader` and never looks at the JDK's file. CratonVM took the
JDK fallback regardless, which put an explicit INFO level on the ROOT — and
since every logger inherits its nearest ancestor's explicit level, that single
entry became a process-wide INFO floor. It is the reason
`traceCategories(...)` could not have worked even with the level face
implemented, and it also installed a `ConsoleHandler` HotSpot does not have.
`read_configuration_no_arg_impl` now skips step 3, and only step 3 — an
explicitly named `java.util.logging.config.file` is a deliberate act and is
still honoured — when the JBoss manager is active.

## Two divergences remain, both understood

* **`identity.jul_vs_*`** — HotSpot answers `false`, CratonVM `true`. Real
  jboss-logmanager mints a fresh `Logger` wrapper per `getLogger` call over a
  shared `LoggerNode`; CratonVM caches one object per name. CratonVM's is the
  stronger guarantee, and because CratonVM also keys level and handler state by
  NAME rather than by object, every state read agrees across routes on both VMs
  (`level.rereadViaStatic=TRACE` on each). No caller is harmed by being handed
  the same object twice.
* **`root.handlers.initialCount`** — HotSpot 1
  (`io.quarkus.bootstrap.logging.QuarkusDelayedHandler`), CratonVM 0. A
  different SPI: `io.quarkus.bootstrap.logging.InitialConfigurator implements
  org.jboss.logmanager.LogContextInitializer`, which real `LogContext` consults
  per node through `getInitialHandlers(name)` / `getInitialLevel(name)`.
  CratonVM synthesised `LogContext` and never ran that chain, so no
  `LogContextInitializer` provider was ever asked. **CLOSED the same day** —
  see `jboss-logcontextinitializer-spi-not-consulted-20260901-FIXED.md`, which
  also corrects this page's account of the unconfigured JBoss level default.
  Of the two divergences listed here, only `identity.jul_vs_*` is still open.

## Two harness defects found on the way

Neither is in git — `apps/` is wholesale-gitignored — so both are recorded
here rather than in a commit.

* **`apps/quarkus-suite-runner/common.args` had no
  `-Djava.util.logging.manager`.** Confirmed against quarkus's own build rather
  than inferred: `apps/quarkus/build-parent/pom.xml:495` (and
  `independent-projects/parent/pom.xml:401`) set it in surefire's
  `systemPropertyVariables` for every module inheriting them; the hand-rolled
  `CratonRunner` harness never replicated it. Appended, with the pre-change
  file kept beside it as `common.args.orig-before-logmanager-fix-20260901`.
  Note this gap alone is not CratonVM-specific: a minimal repro fails
  identically on stock HotSpot without the flag.
* **44 of its 309 classpath entries did not exist.** They pointed at
  `C:\craton\CratonVM\apps\quarkus\...`, a tree that has since moved to
  `C:\craton\CratonVM1\apps\quarkus\...`; every `target/classes` and
  `target/test-classes` directory in the file was dangling. Repointed — all 309
  entries now resolve. Any run of this harness between the move and 2026-09-01
  was resolving classes from jars in `~/.m2` rather than from the built
  reactor.

## Verification

* `cargo test -p cratonvm-native-builtins --lib`: **4184 pass, 0 fail**
  (`logmanager` alone: 50, up from 44).
* `regression-suite/run.sh`: **79 of 79 scheduled vectors pass**, `RJdkLogging`
  — the tracked JUL vector — among them.
* The probe, all three columns above: HotSpot vs CratonVM before vs after.
* Quarkus core suite (313 classes, JIT on, real JDK, 4 shards, 300 s cap),
  identical harness and `common.args` in every arm, only the binary differing:
  `PASS=173 NOTESTS=64 FAIL=71..72 HANG=1..2 NOSTART=2 ABORTED=1`. **One class
  differs between the arms, and it is not a difference.**
  `JavadocToAsciidocTransformerConfigItemTest` reads HANG in the base arm and
  FAIL 37/22/15 in both fixed arms; run isolated, **every binary produces
  37 found / 22 ok / 15 failed**, and the 15 are
  `ServiceConfigurationError: JRubyAsciidoctor could not be instantiated`,
  unrelated to logging. The base arm's HANG was the 300 s cap under 4-shard
  contention on a class that takes ~200-290 s isolated — a measurement of load,
  not of the change.

  Do not read `sum_class_ms` off these runs as a throughput signal: the same
  313 classes summed 534 s, 414 s and 569 s across three 4-shard runs of the
  same harness. Under contention that number measures the host.

### A per-call cost this session could not measure, and did not pretend to

`isLoggable` is what a logging facade calls at every guarded log site (JRuby's
`isDebugEnabled()` is `isLoggable(FINE)`), and on the JBoss face it had been a
constant `true` — so implementing it honestly puts a name read (which
allocates) and a lock on a path that previously had neither.
`no_explicit_logger_levels` takes the answer before the receiver's name is read
whenever nothing anywhere is configured, which is almost every process.

That guard is justified by the shape of the work, **not** by the A/B that
prompted it. The first isolated pair on the JRuby/Asciidoctor class read
198 s base / 291 s fixed and looked like a 47% regression; two more interleaved
rounds read 338/470 and 223/237, and a later pair with the guard in place read
429 s base / 268 s fixed — i.e. the arms cross, and the same binary spans
198-526 s. `Get-Process` sorted by CPU named the reason at the top of the list:
this is a daily-driver desktop and Overwatch was running, alongside another
session's `cratonvm` build. The honest conclusion is that this workload cannot
resolve a per-call cost on this host, not that the cost is zero or that the
guard removed it. A claim about `isLoggable` throughput needs a quiet host and
a workload whose variance is smaller than the effect; neither was available
here.

Two test-harness defects had to be fixed for the new cases to mean anything,
and both had been quietly weakening the existing ones:

* `reset_state_for_tests` did not clear `logger_explicit_levels`, so one test's
  `setLevel` remained the inherited threshold for every later test's descendant
  loggers — order-dependent results in the one table whose entire purpose is to
  be consulted by name.
* the `make_level` test helper never actually set a level VALUE. The mock has
  no field-slot entry for `java/util/logging/Level`, so both its
  `set_field_by_name` calls wrote nowhere and the matching read answered
  `Value::Int(0)` — which the production code accepts as a perfectly good level
  of zero. Every `Level` that helper built was level 0 whatever the caller
  asked for. It went unnoticed because no test using it had ever asserted on
  the value, only on "did not throw". Now declared through
  `set_declared_fields`.

## Related

* `quarkustestprofileawareclassorderer-not-a-hang-throughput-gap-20260817.md`
  — public, still open; owns the ShrinkWrap / `TestResourceManager` throughput
  question this page's last section used to misreport as a hang.
* `loggingsetuprecorder-nosuchmethoderror-at-classpath-scale-20260817.md` — the
  prior blocker in the same bootstrap chain, retired to this directory on the
  same day as this page. Note the correction that retirement carried: the
  binary that page ran was five days stale, but the defect it saw was real —
  the Quarkus logging native had written down its own copy of
  `LoggingSetupRecorder.initializeLogging`'s descriptor, and `70248949c`
  (2026-08-13) is the fix. The "a stale binary, not a live defect" wording
  this page carried until now was true of 08-17, not of the bug.
* `jboss-logcontextinitializer-spi-not-consulted-20260901-FIXED.md` — filed
  from this page's "what remains" and closed the same day. Read it for the
  correction to this page's claim about the unconfigured JBoss level default:
  the `Integer.MIN_VALUE` cited here was Quarkus's `InitialConfigurator`
  configuring the ROOT, not jboss-logmanager's own default, which is INFO.
* `../wildfly/wildfly-jboss-logmanager-geteffectivelevel-null-loggernode.md` —
  an earlier gap in the same native family, and the reason `getEffectiveLevel`
  was a constant in the first place.
