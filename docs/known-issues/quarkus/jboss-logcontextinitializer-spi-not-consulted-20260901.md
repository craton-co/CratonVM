# `org.jboss.logmanager.LogContextInitializer` providers are never asked, so a JBoss logger is born with no initial handlers or level

**Status: characterized and measured, not fixed.** Found 2026-09-01 while
closing the residual of the (now retired) jboss-logmanager `getLogger` cast
report; dev base `56d6c3722`. This is the ONE line of the JBoss LogManager A/B
probe that still differs after that work, and it is a different mechanism from
everything that page covers — it is an application-SPI gap, not logger
plumbing.

## The measurement

`apps/quarkus-suite-runner/probes/JbossLogManagerProbe.java`, Temurin
25.0.3+9 vs CratonVM, same classpath, both under
`-Djava.util.logging.manager=org.jboss.logmanager.LogManager`:

```
                                HotSpot                                        CratonVM
root.handlers.initialCount      1                                              0
root.handlers.initialClasses    io.quarkus.bootstrap.logging.QuarkusDelayedHandler;   (empty)
```

Every other line of that probe — the cast, handler add/remove/set/get, child →
root record delivery, `getLevel`/`setLevel`/`isLoggable`/`getEffectiveLevel`,
ancestor level inheritance, `getParent`, `getLoggerNames` — now matches HotSpot
exactly. This one does not.

## Where HotSpot's handler comes from

Not from a configuration file, and not from
`LogManager.readConfiguration()`. `quarkus-bootstrap-runner` ships two SPI
providers, and they do different jobs:

* `META-INF/services/org.jboss.logmanager.ConfiguratorFactory` →
  `io.quarkus.bootstrap.logging.EmptyLogConfiguratorFactory`, priority 50. Its
  `LogContextConfigurator.configure(LogContext, InputStream)` body is literally
  `return` — it exists to *suppress* the default property configurator.
* `io.quarkus.bootstrap.logging.InitialConfigurator implements
  org.jboss.logmanager.LogContextInitializer`, which declares
  `getInitialHandlers(String)`, `getInitialLevel(String)`,
  `getMinimumLevel(String)` and `useStrongReferences()`, and holds
  `public static final QuarkusDelayedHandler DELAYED_HANDLER`.

Real `org.jboss.logmanager.LogContext` consults the second one **per
`LoggerNode`, at node-creation time** — that is what puts
`QuarkusDelayedHandler` on the root logger before any application code runs.
It is the handler quarkus's own logging setup later replaces once the real
configuration is known.

## Why CratonVM has none

CratonVM synthesises `org/jboss/logmanager/LogContext`
(`native-builtins/src/logmanager.rs`, `jboss_log_context_singleton` /
`get_or_create_jboss_logger`) rather than running the class's bytecode, so
`LogContext`'s node-creation path — the only caller of
`LogContextInitializer` — never executes. No provider is ever loaded, asked,
or cached. The gap is total, not partial: it is not that the wrong initializer
wins, it is that `ServiceLoader.load(LogContextInitializer.class, ...)` is
never called at all.

The same is true of the `ConfiguratorFactory` chain
(`LogManager.doConfigure`), which CratonVM also short-circuits. That one
currently costs nothing observable, because CratonVM's own
`readConfiguration` no longer imports the JDK's
`$java.home/conf/logging.properties` under the JBoss manager and quarkus's
factory configures nothing anyway — but a deployment whose factory *does*
configure something would be silently ignored the same way.

## What it costs

Bounded, and so far only in shape rather than in outcome:

* A quarkus test class that reads `rootLogger.getHandlers()` gets `[]` where
  HotSpot gets one handler. `AbstractQuarkusExtensionTest` stashes that array
  and restores it in `afterAll`, so an empty stash restores consistently and
  its own `InMemoryLogHandler` capture works either way (verified: probe
  `root.capture.count=2` on both VMs).
* Anything that expects records emitted *before* quarkus installs its real
  logging configuration to be buffered by `QuarkusDelayedHandler` and replayed
  afterwards will lose them on CratonVM. Not yet observed to fail a test — the
  313-class quarkus core suite is byte-identical with and without the
  surrounding fixes — but it is the obvious next symptom, and it is the same
  bootstrap chain as
  `loggingsetuprecorder-nosuchmethoderror-at-classpath-scale-20260817.md`.

## What a fix would have to do

Consult the SPI where real `LogContext` does — from
`get_or_create_jboss_logger`, once per new node:

1. resolve a single `LogContextInitializer` (real jboss-logmanager takes the
   first `ServiceLoader` result, falling back to its own `DEFAULT`), and cache
   it per VM, including a cached "there is none" so the miss is paid once;
2. call `getInitialLevel(name)` and record it through
   `record_jul_logger_level`, and `getInitialHandlers(name)` and add each
   through `native_jul_logger_add_handler`.

Two hazards to design against, both of which are why this was filed rather
than attempted in the same session as the plumbing fixes:

* **Re-entrancy.** Node creation would call into application bytecode
  (`InitialConfigurator.<clinit>` constructs a `QuarkusDelayedHandler`) that
  is free to log, i.e. to demand another logger. A guard is needed, and the
  first logger created is created very early in boot.
* **Blast radius.** Every jboss-logmanager consumer in the corpus — WildFly,
  Keycloak, quarkus — would start running a provider it currently does not.
  The change wants those suites run, not just the quarkus one.

## Related

- `jboss-logcontextinitializer-spi-not-consulted-20260901.md`'s parent
  investigation is the retired jboss-logmanager `getLogger` cast report; that
  page carries the full before/after probe table and the six plumbing causes
  fixed alongside this finding.
- `loggingsetuprecorder-nosuchmethoderror-at-classpath-scale-20260817.md` —
  same quarkus logging-bootstrap chain.
