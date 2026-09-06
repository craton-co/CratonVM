# Two Spring Boot classes keep one collector-specific failure each, after the twelve-residual fixes

| | |
|---|---|
| **Status** | OPEN, two independent items. **A** is reproducible on one collector; **B** is a ~50% intermittent on one collector. Neither is a regression of the four fixes: A reproduces on a pristine-dev binary too (control below). |
| **A** | `integration.autoconfigure.IntegrationAutoConfigurationTests` — 1 of 34 fails on **G1 only**, 4 of 4 runs, and the failing METHOD and exception both change from run to run. `PASS 34/34` on ZGC and generational, and `PASS` in the runner's HotSpot baseline. |
| **B** | `kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics` — `Expecting value to be true but was false` (an embedded-broker round-trip latch) on the **generational** collector, 1 of 2 runs alone and 1 of 1 in a full-list sweep; `PASS 3/3` on ZGC and G1 in the same sweep. |
| **Likely shared root (A only)** | the G1-only crash family in `../tomcat/g1-evac-forwarding-assert-and-three-sigsegv-clusters-20260905.md`. Not established — see "Why that page and not a new root cause". |

These are what is left of two rows of the retired twelve-unclustered Spring
Boot residuals write-up, and in both cases **the method that page recorded now
passes** — what remains is a different method of the same class:

* that page's row for `IntegrationAutoConfigurationTests` was
  `explicitIntegrationComponentScan` failing through a
  `BeanPostProcessor before instantiation` chain. It passes. Item A below is
  three other methods, none of them that one.
* its row for `KafkaAutoConfigurationIntegrationTests` was `testStreams`, which
  was the `Properties.clone()` defect and is fixed. Item B is
  `testEndToEndWithRetryTopics`, which that page never recorded as failing.

They are filed together because the shape is the same in both — **one method,
one collector, and green everywhere else** — and separating them into two pages
would say more than the evidence does about whether they are two problems.

## A — `IntegrationAutoConfigurationTests`, G1 only

## The three signatures, all from the same class on the same day

Each is one run of the class alone, `-Parallel 1`, JIT on, `--XX:UseGc G1`,
against `jdk-25.0.4+7`. Every one of them is a **reflection-metadata read that
came back null, empty, or naming the wrong class**, and every one of them
aborts an `ApplicationContext` start rather than throwing where it happened:

```text
integrationGlobalPropertiesUserBeanOverridesAutoConfiguration
  BeanCreationException: Error creating bean with name 'integrationMessageHandlerMethodFactory':
    Could not resolve matching constructor on bean class [null]
                                                          ^^^^^^ the bean's class is null

integrationGlobalPropertiesUserBeanOverridesAutoConfiguration
  BeanDefinitionStoreException: Failed to read candidate component class:
    file [.../IntegrationJdbcProperties.class]
  Caused by: AnnotationConfigurationException: Attribute 'prefix' in annotation
    [org.springframework.boot.context.properties.ConfigurationProperties] is declared as an
    @AliasFor nonexistent attribute 'value' in annotation [java.lang.annotation.Annotation]
                                                           ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ annotationType()
                                                                              answered the base interface

integrationGlobalPropertiesUserBeanOverridesAutoConfiguration
  BeanCreationException: Error creating bean with name 'errorChannel'
  Caused by: SpelEvaluationException: EL1004E: Method call: Method
    getIntegrationProperties(org.springframework.beans.factory.support.DefaultListableBeanFactory)
    cannot be found on type org.springframework.integration.context.IntegrationContextUtils
```

The third is worth reading closely, because it is the one that says what the
other two only imply. `IntegrationContextUtils.getIntegrationProperties` is
`public static ... (org.springframework.beans.factory.BeanFactory)` — `javap`
on the jar the run used confirms it — and SpEL's `ReflectiveMethodResolver`
finds it by walking `type.getMethods()`. So `getMethods()` on a live class
answered a set that did not contain a public static method the class declares.

A fourth signature, from the same class on the PRISTINE-dev binary, is
`Mockito cannot mock this class: class org.springframework.jmx.export.MBeanExporter`
— the retired page's defect 4 (a phi's edge publish clobbering a live register),
which is fixed. It is listed here only so a reader diffing old logs against new
ones does not count it twice.

## Axes

Measured on `dev` merged at 2026-09-06, `-Parallel 1`, one class per run.

| Arm | Result |
|---|---|
| ZGC, JIT on | PASS 34/34 (×3 sweeps) |
| Generational, JIT on | PASS 34/34 (×2 sweeps) |
| **G1, JIT on** | **FAIL 34/1** (×4 sweeps, three different methods/signatures) |
| G1, JIT on, PRISTINE dev binary (control) | FAIL 34/1 (×2) — **so this is not a regression of the four fixes** |
| HotSpot `jdk-25.0.4+7`, runner baseline | PASS |

Two axes are NOT cleanly measured and should not be read from the table:

* **G1 + `--nojit`.** Both attempts hung, and both ran while a `cargo build`
  had the box — this class starts servers and binds ports, and it HANGs
  whenever it is run concurrently with anything heavy (two concurrent runs of
  the class hang each other, which is how that was established). Re-measure on
  a quiet host before treating the JIT as part of the condition.
* **Rate.** "One failure per run" is 4 of 4 G1 runs, not a rate over dozens.

## Why that page and not a new root cause

The G1-only crash family in
`../tomcat/g1-evac-forwarding-assert-and-three-sigsegv-clusters-20260905.md`
lists four fault clusters, two of which are `cratonvm_types::flags::runtime_var_os`
and `cratonvm_types::field_layout::VersionCache::find`. A SIGSEGV taken from
this same twelve-class Spring Boot list on the G1 arm — in
`KafkaAutoConfigurationIntegrationTests`, one run, not reproduced since —
symbolised into the same neighbourhood (`OnceLock::get_or_init` around
`field_layout::scan_cache_enabled`), on `[rsi+r9*8]`, and the VM's own report
named the shape outright:

```text
#  fault addr is inside a RECENTLY DECOMMITTED heap span: base=0x71e6d0e00000
#    len=0x14800000 site=unbumped-middle
#    *** and NOT re-committed since. Something TOUCHED a span the collector
#        proved dead: either a stale pointer read it, or a writer wrote into it. ***
#  fault pc is inside a LIVE registered code buffer
```

A stale pointer into a dead span is precisely the mechanism that would make a
metadata read answer the wrong class, or null, or an empty method array —
without crashing, when the dead span happens to still hold plausible bytes.
That is the whole of the argument, and it is an argument from shape, not a
measurement: **nobody has shown that this class's three failures and those
crashes have one cause.** What would settle it is a G1 kill-switch A/B (the
same one that page lists as un-attempted) run against this class, which is
cheap here because this class fails in four minutes and does not need the
640-class Tomcat sweep.

## B — `KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics`, generational only

```text
=> org.opentest4j.AssertionFailedError: Expecting value to be true but was false
   at ...KafkaAutoConfigurationIntegrationTests.testEndToEndWithRetryTopics
```

The class starts an `@EmbeddedKafka` broker, publishes, and waits on a latch;
the assertion is the latch's own `await` returning false, i.e. the round trip
did not complete inside its window. The other two methods in the class — the
`testStreams` this page's predecessor recorded, and `testEndToEnd` — pass in
every arm.

| Arm | Result |
|---|---|
| ZGC, JIT on | PASS 3/3 |
| G1, JIT on | PASS 3/3 |
| **Generational, JIT on** | FAIL 3/1 in the full-list sweep; alone, **1 FAIL and 1 PASS in 2 runs** |

Two things have to be ruled out before this is called a VM defect, and neither
has been: the host it was measured on was at **97–100% disk** for the whole
window (the broker writes its log segments to `/tmp`, and a slow write is
exactly what a latch timeout looks like), and the generational arm ran last in
a three-arm sequence. Re-measure on a host with room before spending anything
on the collector.

## Reproducer

```bash
# on azureuser@20.80.105.49
printf 'module\tclass\nmodule/spring-boot-integration\torg.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests\n' > /tmp/intg.tsv
cd /data/cratonvm/apps/spring-boot-suite-runner
pwsh -NoProfile -Command "& ./run-spring-boot-suite.ps1 -RunName intg -Exe <binary> \
  -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 -ClassList /tmp/intg.tsv \
  -Parallel 1 -CratonArgs @('--XX:UseGc','G1')"
```

Run it ALONE. This class binds ports and starts an embedded broker; anything
else heavy on the box turns the failure into a HANG and tells you nothing.
