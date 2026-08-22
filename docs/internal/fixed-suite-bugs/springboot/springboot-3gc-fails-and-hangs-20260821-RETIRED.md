# Spring Boot 3-collector re-run, 2026-08-21 — the FAIL/HANG table was one JIT defect and one missing timeout table

**Status: RETIRED — 2026-08-22.** Every row of the reported table is closed:
one real, deterministic VM defect (fixed), two rows that do not reproduce in
isolation on either binary, and nine rows that were the shard harness reporting
slow-but-healthy classes as hangs (fixed, with the budget table now shared
between both runners). One throughput residual is split out to its own open
page and named below.

The reported table, three arms (Generational / G1 / ZGC), one process per
class, the ad-hoc `run-sb-gc-shard.sh` at `PARALLEL=3` and a flat 300 s
per-class timeout:

| | reported |
|---|---|
| **FAIL** (2–3 per arm) | `RabbitAutoConfigurationTests`, `BatchJdbcAutoConfigurationTests`, and `ZipContentTests` (Generational only) |
| **HANG** (8–9 per arm, identical set) | `CacheAutoConfigurationTests`, `ConfigurationPropertySourcesTests`, `FlywayAutoConfigurationTests`, `JacksonAutoConfigurationTests`, `JettyServletWebServerFactoryTests`, `PulsarAutoConfigurationTests`, `TomcatServletWebServerFactoryTests`, `WebMvcAutoConfigurationTests` (+ `KafkaAutoConfigurationIntegrationTests` on Generational) |

"Consistent across all three arms, not GC-specific" was the right reading, and
it is what makes the rest of this page short: a set that does not move with the
collector is not a collector story, and — as it turned out — mostly not a VM
story either.

## Measurement conditions

Azure `vm1`, JDK 25 at `/data/toolchain/jdk-25`, fixture
`/data/cratonvm/apps/spring-boot`, one process per class, `-Xmx2g`. Every
correctness result below (pass/fail, test counts) is load-independent. Every
**wall time** in this page was taken serially with the host's 1-minute load
average between **1.0 and 3.5**; where a table was taken under heavier load it
says so and only its ratios should be read. This box is shared and was observed
at load **178** on 8 cores during this work, so a timing arm run without
checking `/proc/loadavg` first measures the other tenants.

## The HotSpot control comes first

JDK 25 (`/data/toolchain/jdk-25`), the same fixture, the same
one-process-per-class launch, serial. **All twelve classes are clean on
HotSpot**, so nothing here is a broken fixture or a missing `src/` tree — the
trap that turned a 20-class `configuration-metadata` cluster into zero VM
defects, documented in
`../configuration-metadata-20-class-cluster-was-a-missing-src-tree-20260820.md`.

| class | HotSpot |
|---|---|
| `RabbitAutoConfigurationTests` | 76 tests, 0 failed |
| `BatchJdbcAutoConfigurationTests` | 32, 0 |
| `ZipContentTests` | 29, 0 |
| `CacheAutoConfigurationTests` | 59, 0 |
| `ConfigurationPropertySourcesTests` | 11, 0 (1 skipped) |
| `FlywayAutoConfigurationTests` | 73, 0 |
| `JacksonAutoConfigurationTests` | 162, 0 |
| `JettyServletWebServerFactoryTests` | 116, 0 (2 skipped) |
| `PulsarAutoConfigurationTests` | 74, 0 (2 skipped) |
| `TomcatServletWebServerFactoryTests` | 133, 0 |
| `WebMvcAutoConfigurationTests` | 97, 0 |
| `KafkaAutoConfigurationIntegrationTests` | 3, 0 |

## 1. `BatchJdbcAutoConfigurationTests` — a real JIT defect, FIXED

The only genuine correctness row. Deterministic: `failed=1` on 3/3 re-runs and
on all three collectors, always
`testDefinesAndLaunchesLocalJob`, always the same assertion —

```
Expecting: <Unstarted application context …[startupFailure=BeanCreationException]>
to have a single bean of type <org.springframework.batch.core.launch.JobOperator>
but context failed to start:
  BeanCreationException: Error creating bean with name 'jobLauncherApplicationRunner'
  … : No job found with name 'discreteLocalJob'
```

`--nojit` passes 32/32, so it is the JIT. From there:

* the method **passes when run alone** (2.2 s, one test), and passes with
  either half of its eleven predecessors — so it needs the cumulative warm-up,
  not any particular earlier test;
* the compiled-method set differed by **553 methods** between a reproducing
  12-method run and a passing 7-method one. A ten-round binary search over that
  set with `CRATONVM_JIT_DENY` named exactly one:
  `BatchObservabilityBeanPostProcessor.postProcessAfterInitialization`.
  Denying **only** that method passes.

That method is a pass-through `BeanPostProcessor`: `return bean;` on every
path. In this slice nothing auto-configures an observation registry, so its
`getBean(ObservationRegistry.class)` throws `NoSuchBeanDefinitionException`
every time and the catch path is the *hot* path.

Instrumenting the consumer (a classpath shadow of
`JobLauncherApplicationRunner` that prints what it was injected with) gave the
answer directly:

```
JLAR_DIAG jobName=discreteLocalJob jobs.size=0 jobsClass=java.util.Collections$EmptySet
          namesForJob=[] … | discreteJob -> …BatchObservabilityBeanPostProcessor  isJob=false
```

The `discreteJob` singleton **is the post-processor**. The compiled method
returned `this` (local 0) instead of `bean` (local 1), Spring stored that as
the bean, `getBeanNamesForType(Job.class)` then found nothing,
`@Autowired(required = false) setJobs` never fired, and `jobs` stayed at its
field initialiser `Collections.emptySet()` — which is why the message is "no
job found" rather than a type error.

### Root cause

A VM-side probe on the exceptional-return path printed the decoded incoming
arguments the callee's handler frame was rebuilt from:

```
ARGPROBE …postProcessAfterInitialization(Ljava/lang/Object;Ljava/lang/String;)…
  invoke_kind=2 args_slice_len=3 decoded=3
  [0]=obj@0x2004ccb3300 cid=ClassId(2080)
  [1]=obj@0x2004ccb3300 cid=ClassId(2080)      <-- the receiver again
  [2]=null
```

`jit_service_callee_deopt` reads `num_args` **consecutive** 8-byte slots from
the pointer it is handed. The shared hashed/vtable megamorphic stub was
`LEA`ing `arg_offsets[0]` for that pointer. That is correct for the single-pass
backend, which stages its outgoing arguments into one descending block and
passes those offsets; it is wrong for the IR lowerer, which passes each
argument's own register-allocated **home slot**. Those are not contiguous, so
the helper read neighbouring frame words as arguments 1..n.

The IR cascade already stages the same arguments into `args_stage_top_off` for
its MIC/PIC hit service — the megamorphic stub simply was not pointed at it.

**Fixed in `706b104e2`.** `emit_callee_deopt_check` now takes the staging base
explicitly, and emits no service call at all when the caller has no contiguous
block to name (which reproduces the pre-service behaviour: the sentinel keeps
propagating). Two unit tests added; the first was verified to fail on the
pre-fix line by reverting just that line.

| | ZGC | Generational | G1 |
|---|---|---|---|
| before | 32 / **1 failed** | 32 / **1** | 32 / **1** |
| after | 32 / 0 | 32 / 0 | 32 / 0 |

`cratonvm-jit` 2087, `cratonvm-vm` 2577, `cratonvm-gc` 1684 `--lib` tests pass.

### The levers, for whoever meets this shape again

Masked it (each removes the IR megamorphic stub, or the tier that emits it):
`--nojit`, `CRATONVM_C2_SUPERSEDE=0`, `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`,
`CRATONVM_JIT_IR_DIRECT_CALL=0`, `CRATONVM_JIT_IR_CALL_VIRTUAL=0`.

Did **not** mask it, and each one is a lead this page can save the next reader:
`CRATONVM_JIT_LOCAL_HANDLERS=0` (the obvious suspect — a compiled frame running
its own catch — and it is not this), `LOCAL_REGS=0`,
`ENABLE_CALLEE_SAVED_GPR_LOCALS=0`, `INLINE_GETFIELD=0`,
`DISPATCH_CACHE_DIRECT_ENTRY=0`, `INLINE_CALL_DISPATCH=0`, `MAIN_INLINE=0`,
`LICM=0`, `DIRECT_EXC_TABLE_PUBLISH=0`, `GUARDED_VIRTUAL_INLINE=0`,
`IR_SELFREC_DIRECT=0`, `IR_CALL=0`, `IR_CALL_SPECIAL=0`, `IR_ISEL_EMIT=0`,
`IR_LINEAR_SCAN=0`, `IR_UNRESUMABLE_TRAP_GUARD=0`, `FORCE_C2=1`,
`CRATONVM_ZGC_CONC_START=100`, and an **8 GB heap** — which is what rules out a
lost-root reading and leaves plain codegen.

Three standalone repros of the bytecode shape (including one using the real
class, and one instance-method version) all **failed to reproduce**. The
defect needs the site to have gone megamorphic *and* the method to have reached
the optimizing tier; a hand-written hot loop gives neither. The shadow-class
instrumentation that split the body into a wrapper plus an inner method also
made it disappear. What worked was leaving the method alone and instrumenting
its *consumer*.

## 2. `RabbitAutoConfigurationTests` and `ZipContentTests` — do not reproduce

Run serially, one process per class, on **both** binaries — current `dev` and
the 2026-08-19 build the sweep itself used — and on all three collectors:

| class | current `dev` | 2026-08-19 sweep binary |
|---|---|---|
| `RabbitAutoConfigurationTests` | 76/0 × ZGC, Generational, G1 | 76/0 × ZGC, Generational, G1 |
| `ZipContentTests` | 29/0 × ZGC, Generational, G1 | 29/0 × ZGC, Generational, G1 |

Twelve clean runs, no failures. Neither is a regression and neither was fixed
in between — they simply do not fail on their own.

`ZipContentTests` already has a documented intermittent
`OutOfMemoryError: Java heap space` under memory pressure — see the run table
in `zipcontenttests-gc-pressure-theory-REFUTED-20260810.md`, and the tracked
`apps/spring-boot-suite-runner/linux-oracles/zipcontent-class-flake-rate.sh`
written to measure its rate. It
writes a Zip64 archive; three concurrent `-Xmx2g` JVMs on an 8-core box is
exactly the condition that page describes. A shard-only FAIL for this class is
a known event, not a new one.

## 3. The nine HANGs — the harness, and one real throughput residual

**None of the nine is a hang.** Given a budget that fits, all nine pass
serially under ZGC with test counts identical to HotSpot — and eight of them
were since confirmed on Generational and G1 too (see below):

| class | HotSpot | CratonVM (ZGC, serial) | ratio |
|---|---|---|---|
| `CacheAutoConfigurationTests` | 18.4 s | 95.6 s | 5.2× |
| `ConfigurationPropertySourcesTests` | 2.4 s | **598.7 s** | **245×** |
| `FlywayAutoConfigurationTests` | 6.1 s | 72.2 s | 11.9× |
| `JacksonAutoConfigurationTests` | 4.5 s | 106.6 s | 23.7× |
| `JettyServletWebServerFactoryTests` | 13.5 s | 119.2 s | 8.8× |
| `PulsarAutoConfigurationTests` | 3.9 s | 83.7 s | 21.4× |
| `TomcatServletWebServerFactoryTests` | 23.1 s | 192.8 s | 8.3× |
| `WebMvcAutoConfigurationTests` | 5.0 s | 161.1 s | 32.5× |
| `KafkaAutoConfigurationIntegrationTests` | 9.9 s | 21.4 s | 2.2× |

Eight of the nine finish **well inside** the 300 s the shard gave them. They
were reported as hangs because the shard ran three at a time on an eight-core
shared box, and because the driver used a flat 300 s.

That flat timeout is the defect. `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1` has carried a
validated per-class budget table since 2026-07-17 — `CacheAutoConfigurationTests`
600 s, `WebMvcAutoConfigurationTests` 7200 s,
`ConfigurationPropertySourcesTests` 5400 s — **with an entry for every one of
the nine**, each measured by running the class standalone to completion. The
ad-hoc shell driver that ran the Linux sweeps reproduced the launch faithfully
and dropped the table.

**Fixed in `2312c5fd6`**: the budgets move to
`apps/spring-boot-suite-runner/.suite/class-timeouts.tsv`, the PowerShell
function overlays that file on its inline table, and the Linux driver becomes a
tracked file (`apps/spring-boot-suite-runner/linux-oracles/run-sb-gc-shard.sh`)
that loads the same rows and
complains loudly when it cannot find them. It also now records, per row, the
budget a HANG actually hit, flags a HANG that hit only the *base* default (i.e.
one nobody ever validated), flags a PASS landing within 15 % of its own budget
as NEAR-CAP, and prints `/proc/loadavg` at the top — because a fixed per-class
budget on a contended host manufactures HANG rows, and that belongs in the log
rather than in a later reconstruction.

One stale key surfaced while porting: the 900 s budget for
`module/spring-boot-amqp|…amqp.autoconfigure.RabbitAutoConfigurationTests` has
matched nothing since the fixture renamed that module to `spring-boot-rabbitmq`,
so that class has been running on the 300 s default. All 27 rows are now checked
against the current class index; that was the only dead one.

### The one row that is genuinely slow

`ConfigurationPropertySourcesTests` at 245× is the real residual, and it is not
spread across the class: eight of its eleven tests are *faster* on CratonVM
than on HotSpot (they are startup-dominated). The whole wall is three
throughput tests — `environmentPropertyAccessWhenImmutableShouldBePerformant`,
`environmentPropertyAccessWhenMutableWithCacheShouldBePerformant`,
`descendantOfPropertyAccessWhenMutableWithCacheShouldBePerformant` — which do
1000 lookups across 100 property sources. It **passes** all three assertions;
it is only the absolute wall that is out.

Two readings were checked and refuted before this was written up:

* **"the Spring cache never hits under CratonVM."** The dispatch tally shows
  `updateCache` 101 870 times, `Instant.now()` 101 471, `SoftReference.<init>`
  101 388 — one rebuild per access. That is Spring's own semantics for a
  *mutable* source: `SoftReferenceConfigurationPropertyCache.hasExpired()`
  returns true whenever `timeToLive == null`, which is the default. HotSpot
  does the same work.
* **"`SoftReference.get()` is broken."** Probe on both VMs: 0 nulls in 300 000
  reads of a strongly-held referent, identity preserved. Not it.

What it *is*: `perf record` puts **96.09 % of samples inside the cratonvm
binary** — VM runtime, not compiled code — with the leaves spread thinly across
the native-builtins field-access and native-collections machinery
(`ZObjectStarts::contains` 7.9 %, `is_object_address` 7.0 %,
`resolve_field_index_by_class_id` 5.2 %, `read_native_pin` 4.0 %,
`resolve_field_descriptor_byte_cached` 3.9 %, `coerce_field_value_for_slot`
2.6 %, `native_map_put_evict_pinned` 2.3 %, `pin_native_root` 2.2 %, and
`set_field_by_name` / `get_field_by_name` with their `memcmp` under them). No
single term is worth more than ~8 %, so there is no one fix here — this is the
native-collections floor, and it is split out to
`docs/known-issues/perf/springboot-configurationpropertysources-native-collections-floor-20260821.md`
rather than left implied by a ratio on this page.

## The closing run: both arms, the sweep's own conditions

Twelve classes, `PARALLEL=3`, base timeout 300 s — the shard's own settings —
on the fixed binary, on a **quiet** host (load 1.7 at start), using the tracked
driver so what is measured is what shipped:

| arm | result |
|---|---|
| **A** — budget table deliberately absent | **PASS 11, HANG 1** |
| **B** — budget table present | **PASS 12, HANG 0, FAIL 0** |

Arm A's single HANG is `ConfigurationPropertySourcesTests`, and the driver
labels it for what it is: `budget 300s (base default -- no validated budget for
this class)`. Every other class — including all three reported FAIL rows —
passes at parallel-3 with 300 s **when the host is idle**. Arm B's per-class
walls, against the budgets they were given:

```
BatchJdbcAutoConfigurationTests             PASS    46s  cap  300s
ZipContentTests                             PASS    53s  cap  300s
RabbitAutoConfigurationTests                PASS    87s  cap  900s
CacheAutoConfigurationTests                 PASS   108s  cap  600s
FlywayAutoConfigurationTests                PASS    87s  cap  600s
JacksonAutoConfigurationTests               PASS   125s  cap  900s
JettyServletWebServerFactoryTests           PASS   131s  cap  900s
PulsarAutoConfigurationTests                PASS    69s  cap  900s
TomcatServletWebServerFactoryTests          PASS   131s  cap  900s
KafkaAutoConfigurationIntegrationTests      PASS    18s  cap  600s
WebMvcAutoConfigurationTests                PASS   137s  cap 7200s
ConfigurationPropertySourcesTests           PASS   600s  cap 5400s
```

No NEAR-CAP rows. So the reported table needed **two** things to appear: the
missing budget table *and* a busy host.

### The host was the other half, and it is measurable

The same eight classes, run serially on this box while other tenants had it at
load 20–35, against the same classes on an idle box:

| class | idle | under load 20–35 |
|---|---|---|
| `CacheAutoConfigurationTests` | 95.6 s | 207 s |
| `FlywayAutoConfigurationTests` | 72.2 s | 263 s |
| `JacksonAutoConfigurationTests` | 106.6 s | 325 s |
| `JettyServletWebServerFactoryTests` | 119.2 s | 264 s |
| `TomcatServletWebServerFactoryTests` | 192.8 s | 256 s |
| `WebMvcAutoConfigurationTests` | 161.1 s | 282 s |

Every one of them crosses 300 s under load and none of them does idle. That is
the whole HANG column, and it is why the driver now prints `/proc/loadavg` at
the top of every shard log. This box was seen at **load 178 on 8 cores** during
this work.

## Generational and G1 confirm ZGC

Eight of the nine HANG classes re-run serially on the other two collectors
(`ConfigurationPropertySourcesTests` excluded — it is the known throughput
outlier, collector-independent, and costs an hour per arm):

**G1: 8/8 PASS.** **Generational: 7/8 PASS**, with counts identical to HotSpot
on every one.

The eighth is worth its own note, because it is the `+ Kafka on Generational
only` in the reported table. `KafkaAutoConfigurationIntegrationTests` failed
**2/3** on the Generational arm — but that arm ran at load 20–35 and took
282 s, and the two failures are both

```
KafkaAutoConfigurationIntegrationTests.java:93
  assertThat(listener.latch.await(30, TimeUnit.SECONDS)).isTrue();
```

— a **30-second wall-clock deadline inside the test**, on an embedded broker,
on a box running 4× oversubscribed. Re-run on the same collector once the host
was quiet: **3/3 PASS, three times running, 25 s each**. G1 passed it in 20 s.
It is a host artefact against a test's own timing assumption, not a collector
defect — and it is the reason a class can look collector-specific when the only
thing that differs is which arm happened to run while the box was busy.

## What this page is evidence for

* A FAIL/HANG table from a parallel shard is a statement about the shard, not
  about the VM, until each row has been re-run **serially** — three of the
  twelve rows here survived that and one of those three was real.
* Re-run against the **sweep's own binary**, not just current `dev`, before
  saying a row was fixed in between. Two rows here would otherwise have been
  written up as "fixed by someone else"; they were never reproducible.
* A harness that re-implements a launch will re-implement it *minus* whatever
  was learned since. The budget table existed, was validated, and was invisible
  to the driver that needed it.
* **Record the host load beside every wall time.** Half of this table needed a
  busy box to appear at all, and one row (`Kafka` on Generational) is a test's
  own 30-second deadline losing to 4× oversubscription. Neither is visible in a
  results.tsv that carries only pass/fail.
