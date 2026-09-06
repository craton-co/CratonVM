# Twelve unclustered Spring Boot residuals from the fixed-harness 3-GC triage — CLOSED

## Status

**✅ RESOLVED 2026-09-06 — every method this page named now passes.** All
twelve classes start and run; the four defects below account for eleven of them
outright and for the twelfth's recorded symptom as well.

Two classes keep ONE failure each in a method this page never recorded, and
each on ONE collector: `IntegrationAutoConfigurationTests` on G1 (its
`explicitIntegrationComponentScan` passes; three OTHER methods fail, one per
run, with a different reflection-metadata exception each time), and
`KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics` on the
generational collector at about one run in two (its `testStreams` — the row
this page recorded — passes). Neither is a regression of the work here: the
first reproduces on a pristine-dev binary under the same control. Both are
re-filed as `known-issues/springboot/two-collector-specific-residuals-after-the-twelve-20260906.md`,
which also says why the first is probably the G1 crash family's quiet twin
rather than a Spring Boot problem.

The page opened as "OPEN, shallow pass only" with twelve rows recorded from
whatever signature the 2026-09-04/05 run's own log carried, and asked three
questions in its Next-steps section. All three are answered below, and the
answer to the first two is the same in every row: **none of the twelve was an
environment gap, and none of them was twelve separate problems.** Four defects
account for eleven of them.

The original page's own framing was the thing to discard first. Its headline
grouping — "likely one shared cause: missing test infrastructure (4 classes)" —
was wrong: every one of the twelve classes **passes on HotSpot on this host**,
from the runner's own baseline (`.suite/baseline/hotspot-baseline-latest.tsv`,
1991 classes), so nothing was missing from the box. Two of its three remaining
groupings ("Mockito argument-mismatch pair", "six singletons, no attempted
grouping") turned out to be one defect and three.

## What was actually wrong

| # | Defect | Fix | Classes closed |
|---|---|---|---|
| 1 | A redefinition of an ANCESTOR dropped the native shadow of a VM-MINTED carrier, whose fallback body is `java.lang.Object`'s | `redefine_immune_vm_minted_carrier_native` | LoggersEndpoint, Couchbase, ServiceConnectionContextCustomizer, GrpcChannelBuilderCustomizers, GrpcClientAutoConfiguration |
| 2 | `Properties.clone()` rebuilt the clone from the String side table, dropping every non-String value | copy the source's CHM, which is the real body's own second statement | KafkaAutoConfiguration, KafkaAutoConfigurationIntegration, KafkaMetricsAutoConfiguration |
| 3 | `ArrayList.addAll(map.values())` read the view's CAPTURED backing, not the live map | `resync_values_view` on the ARGUMENT, the one reader that never went through the funnel | HibernateJpaAutoConfiguration, DataJpaRepositoriesWithEnversRevision |
| 4 | A phi's edge publish clobbered a register a later edge copy still read its own source out of | `gp_reg_owner`: publishing into a register marks the previous owner unreadable | (a regression that landed the same day; it re-broke 1, 2 and 5 above with the JIT on) |

`ConfigDataEnvironmentPostProcessorIntegrationTests` is the thirteenth story and
needed no fix: see "The twentieth failure that was not a failure" below.

### 1. A VM-minted carrier has no bytecode to yield to

`vm_exec`'s `native_shadow_dropped_by_redefine` asks, for a receiver class and a
method, whether the body a registered native would yield to (a) exists and (b)
belongs to a class an agent has redefined. It resolves that body with
`find_method_recursive`, which walks past a class that declares nothing of its
own — and a `cratonvm/internal/*` carrier declares nothing of its own, because
no image contains a class file for it. So the walk landed on `java.lang.Object`,
which Mockito's inline mock maker retransforms the first time anything mocks a
class rather than an interface. From that moment the guard read "the body exists
and its class was redefined", dropped the carrier's native, and dispatch fell
through to `Object.equals` — identity.

```text
[native-shadow] cratonvm/internal/UnmodifiableList.equals(Ljava/lang/Object;)Z
                dropped=true probe=Some((ClassId(0), true, 2)) immune=false
[native-shadow] cratonvm/internal/UnmodifiableSet.equals(Ljava/lang/Object;)Z
                dropped=true probe=Some((ClassId(0), true, 2)) immune=false
```

`ClassId(0)` is `java.lang.Object`. Every `Collections.unmodifiableList(..)`,
`List.of(..)`, `Collections.unmodifiableSet(..)` and `Map.of(..)` in the process
compared by IDENTITY from the first `mock()` onward — which is why the symptom
reads as an assertion whose two sides print the same text:

```text
expected: "["test.member"] (SingletonList@d18)"
 but was: "["test.member"] (UnmodifiableRandomAccessList@d15)"
```

The reproducer is `apps/probes/MinAssertRedefine.java`: five seconds, no Spring.
It compares three ways — the receiver's own `equals`, AssertJ's
`StandardComparisonStrategy.areEqual`, and `org.assertj.core.util.Objects.areEqual`
— before and after mocking one abstract class:

```text
---- before mock ----   unmod vs single   plain=true  scs=true  util=true
---- after mock ----    unmod vs single   plain=true  scs=false util=true   <<< DIVERGES
                        listOf vs single  plain=true  scs=false util=true   <<< DIVERGES
                        unmodSet vs Set.of plain=true scs=false util=true   <<< DIVERGES
                        al vs single      plain=true  scs=true  util=true
                        map vs Map.of     plain=true  scs=true  util=true
```

The two rows that stayed right are the tell: `java/util/ArrayList` and
`java/util/LinkedHashMap` were already on the immunity allow-list
(`redefine_immune_synthetic_collection_native`), added when the same shape was
found for the JDK-named synthetic collections. The `cratonvm/internal/*`
carriers were not, and their case is stronger than the allow-listed one: those
classes' real JDK bodies would misread a CratonVM layout, while a carrier has no
real JDK body at all.

The Grpc pair is the same defect one door out. Mockito's `verify()` compares the
recorded argument with the expected one through `equals`, and the argument is a
`Map`, so the failure printed a WANTED and an ACTUAL that are textually
identical:

```text
Argument(s) are different! Wanted:
nettyChannelBuilder.defaultServiceConfig({"healthCheckConfig" = {"serviceName" = "test"}});
Actual invocations have different arguments:
  ... nettyChannelBuilder.defaultServiceConfig({"healthCheckConfig" = {"serviceName" = "test"}});
```

`ServiceConnectionContextCustomizerTests.equalsAndHashCode` is the same again,
two doors out: its `equals` is `this.keys.equals(other.keys)` over a
`List.of(CacheKey)`, and its `hashCode` — which does not go through the dropped
native — matched, which is why the failure printed two objects with the same
identity string.

### 2. `Properties.clone()` kept only the String entries

`native_properties_clone` shallow-copied the receiver, dropped the aliased `map`
reference, and rebuilt the clone's backing from `ordered_snapshot_kv` — a
`Vec<(JavaText, JavaText)>`, i.e. the side table, which holds STRING keys and
STRING values only. A `Properties` is a `Hashtable<Object,Object>` and Java puts
non-String values in one freely; those live in the `map` CHM and are what
`chm_extra_entries` exists to read back. They did not survive the clone:

```text
apps/probes/PropertiesCloneProbe.java, --nojit
  after putAll  size=2 keys=[application.id, bootstrap.servers]
  after clone   size=1 keys=[application.id]          HotSpot: size=2
  string-only clone size=2                            (both)
```

`KafkaStreamsConfiguration.asProperties()` is `new Properties()` +
`putAll(configs)` + `clone()`, and `bootstrap.servers` is a `List<String>`. The
clone dropped it and Kafka answered:

```text
ConfigException: Missing required configuration "bootstrap.servers" which has no default value.
```

The page had read that message as evidence of a missing broker. It is not: the
value was present in the map the VM handed to `Properties`, and
`KafkaAutoConfigurationTests` never contacts a broker at all. The instrument
that settled it was a classpath overlay of
`KafkaStreamsAnnotationDrivenConfiguration` printing the map at each hop:

```text
[OVERLAY] after apply   size=1 hasBootstrap=true  value=[localhost:9092, localhost:9093]
[OVERLAY] asProperties  size=1 hasBootstrap=false keys=[application.id]
```

The fix is the real body's own second statement,
`clone.map = new ConcurrentHashMap<>(map)`, done against the source's backing
rather than a rendered snapshot — which also keeps the clone's enumeration order
equal to its source's, the property the retired
`system-properties-clone-enumerates-in-a-different-order-than-its-source`
write-up was about.

### 3. `addAll` read a captured map view

`native_al_add_all`'s fast path reads the ARGUMENT's `(elementData, size)`
straight out of `al_state`. For a `values()` view — which CratonVM caches on its
source map and which is ArrayList-SHAPED — those slots hold the snapshot taken
when the view was built, not the live map. Every other reader of a view's raw
slots opens with `resync_values_view`; this one did not, because the
receiver-side sweep that added those resyncs did not look at arguments.

```text
apps/probes/StaleViewAddAllProbe.java, --nojit, all three map families
  put a,b,c        addAll(m.values())   3
  put d,e          addAll(m.values())   3      <-- HotSpot 5
                   m.values().size()    5
                   m.values().toArray() 5
                   for (x : m.values()) 5
                   new ArrayList<>(m.values()) 5
```

Hibernate's `InFlightMetadataCollectorImpl.collectTableMappings()` is
`new ArrayList<>()` then `addAll(namespace.getTables())`, and `getTables()` is
`tables.values()` on a map that GROWS when Envers contributes its audit tables.
So the foreign-key second pass walked a table set that predated Envers, never
reached `Country_AUD`, and `ForeignKey.referencedTable` stayed null until schema
export dereferenced it:

```text
NullPointerException: Cannot invoke "org.hibernate.mapping.Table.isPhysicalTable()"
  because "this.referencedTable" is null
  at org.hibernate.mapping.ForeignKey.isPhysicalConstraint(ForeignKey.java:147)
  at ...StandardForeignKeyExporter.getSqlCreateStrings
```

Two instruments found it without a debugger, and both are worth reusing.
`Table.setUniqueInteger(i++)` is assigned ONLY by the loop in question, so the
final metadata's unique integers say how far that loop got:

```text
HotSpot   tables=[Country#u0, Country_AUD#u1, FkProbe2Child#u2, FkProbe2Parent#u3, REVINFO#u4]
CratonVM  tables=[Country#u0, Country_AUD#u0, REVINFO#u0]
```

And a custom `hibernate.implicit_naming_strategy` is called once per foreign key
that the pass actually visits, with `getBuildingContext().getMetadataCollector()`
reachable from the callback — so it prints the collector's table set at exactly
the moment that matters. On CratonVM it printed three tables where the namespace
already held five, which is what named `collectTableMappings` rather than the
namespace, the FK map, or Envers.

### 4. A phi's publish clobbered a live register (a same-day regression)

Not one of the twelve, but it re-broke five of them with the JIT on, so it is
recorded here. `perf/ir-defaults-on-20260905` turned thirteen optimizing-tier
switches on by default. Two of them together produced wrong VALUES in compiled
code: `ir-phi-copy-regs` makes an edge's phi copies move register-to-register,
and `ls-carry-relief` moves the allocation onto the shape where that is unsafe.

`emit_copy_op` reads a copy's source out of its register when it has one and
publishes the destination phi into its register from RAX, and argued that
`resolve_parallel_copy`'s ordering covers both: "a register and its home word go
stale at the same point, and an order that protects one protects the other."
That holds while each register belongs to one node. It does not hold across an
edge: a phi's live range BEGINS there, so the allocator may give it the register
of a value whose range ENDS there — and then an earlier copy's publish clobbers a
register a later copy still reads its own source out of. `gp_reg_live` is indexed
by NODE and was never cleared, so `resident_gpr` handed that register back as if
it still held the old value.

The victim was Mockito's inline mock maker. ByteBuddy's shaded ASM writes `ff ff`
as the placeholder for a forward branch and patches it in `Label.resolve` from
`forwardReferences`; the placeholders survived, so CratonVM's own verifier
rejected the retransformed bytes:

```text
retransformClasses0: UnsupportedClassRedefinitionError { class_name: "java/lang/Object",
  message: "new bytes failed bytecode verification: verification error in
  java/lang/Object.wait: ... branch at offset 89 targets 88, which is not an
  instruction boundary" }
MockitoException: Could not modify all classes [...]
```

and every `mock()` of a class failed. `GrpcChannelBuilderCustomizersTests` is a
three-minute reproducer: 4/4 runs dirty under the defaults, 0/8 with either
`CRATONVM_JIT_IR_PHI_COPY_REGS=0` or `CRATONVM_JIT_LS_CARRY_RELIEF=0`, and 0/N
after the fix. The repair is a reverse map from physical register to owning node,
consulted by `mark_gp_reg_live`: publishing into a register marks whoever held it
unreadable, so `resident_gpr` stops lying and the reader falls back to the home
word the copy resolver already keeps correct.

## The twentieth failure that was not a failure

`ConfigDataEnvironmentPostProcessorIntegrationTests` contributed 20 of the run's
FAILs on the G1 and ZGC arms and a `CRASH` (rc=139) on the generational one. It
needed no fix, and the 20 were not twenty problems:

```text
runWhenHasLocalFileLoadsWithLocalFileTakingPrecedenceOverClasspath
  Expecting file: .../core/spring-boot/./application.properties  not to exist
runWhenHasCommandLinePropertiesLoadsWithCommandLineTakingPrecedence
  expected: "frompropertiesfile"  but was: "fromlocalfile"
  ... and 18 more, every one of them "but was: fromlocalfile"
```

That test writes `./application.properties` into the module directory and deletes
it in a `finally`. The generational arm SEGV'd part-way through the class, so the
`finally` never ran, and the file sat in the shared spring-boot checkout:

```text
-rw-rw-r-- 1 azureuser azureuser 58 Sep  4 04:13 .../core/spring-boot/application.properties
    #Fri Sep 04 04:13:53 UTC 2026
    my.property=fromlocalfile
```

Every later arm — and every later RUN — then read that file first. Delete it and
the class passes 87/87. **A suite that writes into the tree under test leaves
poison for the next arm, and a crash is the way it gets left**: the three arms
were concurrent, so the arm that crashed poisoned the two that did not. Two
practical consequences: the runner's per-class results are not independent across
arms when a class writes to the checkout, and any driver that reruns this class
list should delete that file first (this session's does).

The generational SIGSEGV itself is a separate matter and is NOT closed here. All
three of that run's `rc=139` crashes — this class,
`HibernateJpaAutoConfigurationTests` and `KafkaAutoConfigurationIntegrationTests`
— faulted at the SAME pc offset in the same binary, `0xA716AC` relative to the
mapped image, which `addr2line` puts in `ZgcRealHeap::alloc_raw`, on an indexed
load `[rsi+r9*8]` one word into an unmapped region. That binary was
`/data/cvm-h2serial-20260813/target/release/cratonvm`, three weeks old at the
time of the run; the crash does not reproduce on current dev, where those two
classes now pass. Two commits on dev name that exact shape —
`ZGC decommitted granules inside the range it tells the JIT is safe to load raw`
and `the decommit ring named memory that had a new owner` — so the most likely
reading is that it was fixed between the two. Left as a reading, not a verdict:
nobody re-ran the old binary against those commits.

## Answers to the page's own Next-steps

1. **"Confirm or rule out the testcontainers/infra hypothesis."** Ruled out. All
   twelve classes are `PASS` in the runner's HotSpot baseline on this host, the
   three Kafka rows are defect 2, and `KafkaAutoConfigurationTests` and
   `KafkaMetricsAutoConfigurationTests` never touch a broker.
2. **"Re-run each singleton in isolation with full stdout captured."** Done, and
   the truncated "actual" side was where the information was in every case. Two
   of the six singletons were defect 1, one was defect 3, one was harness
   pollution, and one (`IntegrationAutoConfigurationTests`) was a symptom of
   defect 1 with the JIT on, and passes 34/34 on ZGC and the generational
   collector; its remaining G1-only failure is in other methods and is re-filed.
3. **"Cross-check against HotSpot before spending further CratonVM-side
   investigation."** This should have been step 1, not step 3: it took one `awk`
   over a baseline file the runner had already written, and it invalidated the
   page's headline grouping before any of the twelve was opened.

## Verification

Binary: dev merged 2026-09-06 + the four fixes, release.
Oracle: HotSpot `jdk-25.0.4+7` (`/data/jdkimages/jdk25-linux/jdk-25.0.4+7`),
via the runner's own baseline of 1991 classes.
Runner: `apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`, `-Parallel 1`,
JIT on, one full pass of the twelve-class list per collector.

| Class | tests | ZGC | G1 | Generational |
|---|---|---|---|---|
| `ConfigDataEnvironmentPostProcessorIntegrationTests` | 87 | PASS | PASS | PASS |
| `ServiceConnectionContextCustomizerTests` | 2 | PASS | PASS | PASS |
| `LoggersEndpointTests` | 10 | PASS | PASS | PASS |
| `CouchbaseAutoConfigurationTests` | 18 | PASS | PASS | PASS |
| `DataJpaRepositoriesWithEnversRevisionAutoConfigurationTests` | 9 | PASS | PASS | PASS |
| `GrpcChannelBuilderCustomizersTests` | 13 | PASS | PASS | PASS |
| `GrpcClientAutoConfigurationTests` | 33 | PASS | PASS | PASS |
| `HibernateJpaAutoConfigurationTests` | 71 | PASS | PASS | PASS |
| `IntegrationAutoConfigurationTests` | 34 | PASS | **FAIL 1** (re-filed) | PASS |
| `KafkaAutoConfigurationIntegrationTests` | 3 | PASS | PASS | **FAIL 1** (re-filed) |
| `KafkaAutoConfigurationTests` | 54 | PASS | PASS | PASS |
| `KafkaMetricsAutoConfigurationTests` | 4 | PASS | PASS | PASS |

Rust gates: `cargo test` green for `-p cratonvm-jit`, `-p cratonvm-native-collections`,
`-p cratonvm-native-builtins` and `-p cratonvm-types` in DEBUG (so `debug_assert`
is live). `-p cratonvm-vm`'s only failure was
`a_capturing_sam_call_site_dispatches_through_a_thunk`, at 398529 of 800000
dispatches — the engagement-threshold flake `perf/ir-defaults-on-20260905`
filed with its own merge ("failed twice in fourteen runs ... only on a contended
host, at a near-identical 398.6k of 800k"), and this run was on a host whose
disk had just filled. It asserts how QUICKLY an inline cache takes over, not
what anything computes.

## Repro

```bash
cd apps/spring-boot-suite-runner
# the class list is the twelve rows; the leading rm is not optional, see above
rm -f ../spring-boot/core/spring-boot/application.properties
pwsh -NoProfile -Command "& ./run-spring-boot-suite.ps1 -RunName twelve \
  -Exe <binary> -JdkHome <jdk> -ClassList <tsv> -Parallel 1 \
  -CratonArgs @('--XX:UseGc','Z')"
```
