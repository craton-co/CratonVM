# `org.jboss.logmanager.LogContextInitializer` providers are never asked — FIXED 2026-09-01

Retired from `docs/known-issues/quarkus/`, filed and closed the same day. It
was the one line of the JBoss LogManager A/B probe still differing after the
`getLogger`-cast page's residual was closed, and closing it also **overturned a
claim that page made** — see "The default this corrected" below.

## What the page reported

Under `-Djava.util.logging.manager=org.jboss.logmanager.LogManager`, with
`quarkus-bootstrap-runner` on the classpath:

```
                              HotSpot                                    CratonVM
root.handlers.initialCount    1                                          0
root.handlers.initialClasses  io.quarkus.bootstrap.logging.QuarkusDelayedHandler;   (empty)
```

CratonVM synthesises `org/jboss/logmanager/LogContext` rather than running its
bytecode, so `LoggerNode`'s constructor — the only caller of
`LogContextInitializer` — never executed. No provider was loaded, asked, or
cached: `ServiceLoader.load(LogContextInitializer.class, ...)` was never called
at all.

## The fix

`get_or_create_jboss_logger` now consults the SPI once per new node, mirroring
`LoggerNode.<init>` as it is actually written:

```java
effectiveMinLevel = requireNonNullElse(initializer.getMinimumLevel(name), Level.ALL).intValue();
level             = initializer.getInitialLevel(name);         // null => inherit
if (level != null) effectiveLevel = level.intValue(); else effectiveLevel = Logger.INFO_INT;
handlers          = safeCloneHandlers(initializer.getInitialHandlers(name));
```

Discovery mirrors `LogContext.discoverDefaultInitializer0`: `ServiceLoader.load`
over the interface's own class loader, first provider wins, nothing when there
is none. The result — **including the negative** — is cached per VM, because
otherwise every logger creation in a process with no provider pays a full
classpath scan.

`effectiveMinLevel` needed a second name-keyed table.
`LoggerNode.isLoggableLevel` is

```java
level != OFF_INT && level >= effectiveMinLevel && level >= effectiveLevel
```

— two independent thresholds, so a provider that raises the minimum silences a
category `setLevel` alone would have enabled. Collapsing them into one table
passes every other test in the module and loses exactly that;
`the_minimum_level_floor_outranks_an_explicit_set_level` is the test that
refuses it.

That line also carries a third thing no threshold comparison can express:
`isLoggable(Level.OFF)` is **false however low the thresholds are**, because
`Level.OFF.intValue()` is `Integer.MAX_VALUE` — at or above every threshold
there is. The old implementation answered `true`, and did so most confidently
in the case where a caller had just switched a category off.

## The default this corrected

The page this one branched from stated, in a constant, a test name and a
paragraph of prose, that

> an unconfigured jboss-logmanager node sits at
> `effectiveMinLevel = Integer.MIN_VALUE` and logs everything, where an
> unconfigured JUL logger stops at INFO.

**That is false.** The `-2147483648` behind it was measured with
`quarkus-bootstrap-runner` on the classpath, and it is quarkus's own
configuration: `InitialConfigurator.getInitialLevel("")` returns `Level.ALL`
for the ROOT — and only for the root — after which every logger inherits it
down the dotted-name chain.

The control is deleting the provider from the classpath, not reading harder.
Both arms are "jboss-logmanager 3.2.2 under its own LogManager on Temurin 25";
only the SPI differs:

| probe line | with `quarkus-bootstrap-runner` | jboss-logmanager alone |
|---|---|---|
| `root.ownLevel` | ALL | INFO |
| `root.effective` | -2147483648 | **800** |
| `level.freshControl.effective` | -2147483648 | **800** |
| `level.freshControl.isLoggableTrace` | true | **false** |
| `root.handlers.initialCount` | 1 (`QuarkusDelayedHandler`) | 1 (`ConsoleHandler`) |

So `JBOSS_UNCONFIGURED_EFFECTIVE_LEVEL` is `800`, the `Logger.INFO_INT` seed —
identical to JUL — and the divergence the old page named does not exist. One
application's configuration had been written down as the library's default, and
the reason it survived review is that the only classpath anyone had run the
probe on had that application on it.

## Verified both ways

`CRATONVM_JBOSS_LOG_CONTEXT_INITIALIZER=0` restores the previous behaviour. It
exists because this is the one place in the logging natives that runs
APPLICATION bytecode from inside a logger allocator — a provider's `<clinit>`
constructs handlers and is free to log — and WildFly, Keycloak and Quarkus all
reach it. Same binary, same classpath, the flag alone:

| probe line | HotSpot | SPI on | SPI off |
|---|---|---|---|
| `root.handlers.initialCount` | 1 | **1** | 0 |
| `root.handlers.initialClasses` | `QuarkusDelayedHandler` | **`QuarkusDelayedHandler`** | (empty) |
| `root.ownLevel` | ALL | **ALL** | null |
| `root.effective` | -2147483648 | **-2147483648** | 800 |
| `level.freshControl.isLoggableTrace` | true | **true** | false |
| `off.isLoggableAll` / `off.isLoggableOff` | true / false | **true / false** | true / false |

A kill switch that does not visibly move the thing it gates is not a kill
switch; this one moves five lines, in both directions.

## Re-entrancy

The initializer is asked AFTER the new node is inserted into the registry, and
a thread-local guard makes a re-entrant `getLogger` skip the SPI. Both are
load-bearing: Quarkus's `InitialConfigurator.<clinit>` constructs a
`QuarkusDelayedHandler`, and anything on that path may log — which demands a
logger, which re-enters `get_or_create_jboss_logger`. Registering first means
that re-entrant call finds this object instead of building a second one and
recursing; the guard means it does not ask the SPI again while the SPI is still
being resolved.

Every failure path resolves to "no provider": a missing jboss-logmanager, an
unloadable provider class, a `ServiceLoader` that throws. Real jboss-logmanager
also swallows provider failures here (`discoverDefaultInitializer` catches and
falls back to `DEFAULT`), and a VM that refused to hand out loggers because a
logging SPI misbehaved would be worse than one that logs a little less.

## Verification

* `cargo test -p cratonvm-native-builtins --lib`: **4188 pass, 0 fail**
  (`logmanager` alone: 54, up from 50).
* `regression-suite/run.sh`: **79 of 79** scheduled vectors pass.
* `cargo test -p cratonvm-types`: the flag guards pass with the new switch
  declared in `flag_groups.rs` + `flag-surface.txt` and the two generated docs
  regenerated (`render-inventory.sh`, `render-tokens.sh`).
* The probe: every line matches HotSpot except `identity.jul_vs_*` (below).
* Quarkus core suite, 313 classes, 4 shards, 300 s cap, run **twice on the
  same binary with only the kill switch differing** — so the comparison carries
  no cross-binary or cross-commit confound. `PASS=174 NOTESTS=64 FAIL=69..70
  HANG=2..3 NOSTART=2 ABORTED=1`, and exactly **one** class differs:
  `BasicBuildFromWorkspaceModuleTest`, HANG on the SPI-on arm and FAIL 1/0/1
  on the SPI-off arm.

  That one was chased rather than waved at, because "the new code path
  occasionally hangs" is the worst thing this change could be hiding. It is a
  **pre-existing flake in that class, on both arms**:

  | arm | runs | wedges |
  |---|---:|---:|
  | SPI on | 30 | 0 |
  | SPI off | 30 | **1** |

  (60 alternating runs, same binary, 120 s cap; plus 12+12 earlier under four
  CPU spinners, 0 and 0; plus 11 isolated SPI-on runs, 1 wedge.) The rate is
  the same on both sides and the direction reverses between samples — the two
  suite HANGs that started this were the small-sample version of the same
  noise. Sampling the wedged process settles what it is: CPU climbed 123.5 →
  132.0 s over 8 s of wall clock, i.e. it is **burning a core, not blocked** —
  a runaway, not a deadlock — with a **16.8 GB working set under `--Xmx 2g`**.
  That last number is not explained by anything here and is worth its own look;
  it is recorded rather than chased because it reproduces with this change
  disabled.

  Two other classes differed against the *previous* run (a different binary):
  `ErrorPropagationTest` and `JavadocToAsciidocTransformerConfigItemTest`. Both
  answer identically with the SPI on and off when run isolated
  (`PASS 1/1/0` and `37/22/15`), so neither is attributable either — in
  particular the `ErrorPropagationTest` FAIL→PASS is NOT something this fixed,
  and must not be claimed as a win. Overwatch was running throughout and the
  two suite arms took 15m38s and 11m26s for identical work.

## What remains

* **`identity.jul_vs_*`** — HotSpot `false`, CratonVM `true`. Real
  jboss-logmanager mints a fresh `Logger` wrapper per `getLogger` call over a
  shared `LoggerNode`; CratonVM caches one object per name. The stronger
  guarantee, and harmless because CratonVM keys level and handler state by NAME
  rather than by object, so every state read agrees across routes on both VMs.
* **The `ConfiguratorFactory` chain is still short-circuited.** This is the
  OTHER jboss-logmanager SPI (`LogManager.doConfigure` → `ServiceLoader<
  ConfiguratorFactory>` → `LogContextConfigurator.configure`), and the
  no-provider control above measures exactly what it costs: with
  jboss-logmanager alone, HotSpot's root logger carries a `ConsoleHandler` and
  level INFO from `DefaultConfiguratorFactory`, where CratonVM's carries
  neither. Deliberately NOT filed as an open page: no workload in the corpus
  reaches it. Every jboss-logmanager consumer here ships its own factory —
  Quarkus's `EmptyLogConfiguratorFactory` (priority 50, `configure` body
  `return`) beats jboss's default and configures nothing, which is why the
  provider arm shows no difference — and WildFly configures through its own
  subsystem. The path that would show it is "jboss-logmanager standalone with
  no `logging.properties`", which nothing here is. If that changes, the fix has
  the same shape as this one: resolve the factory by lowest `priority()`, call
  `create()`, call `configure(logContext, null)`.

## Related

* The jboss-logmanager `getLogger`-cast report in this directory — the
  investigation this branched from. Its "Two divergences remain" section is
  superseded by this page, and its claim about the unconfigured JBoss default
  is corrected above.
* `quarkustestprofileawareclassorderer-not-a-hang-throughput-gap-20260817.md`
  — public, still open, and unrelated to logging semantics.
