# `OtlpMetricsPropertiesConfigAdapterTests` HANGs — last activity is Mockito inline-mock-maker self-attach / ByteBuddy `Invoker$Dispatcher`

**Status: OPEN — found 2026-07-17 (hypothesis, unconfirmed — no thread dump captured)**

## Symptom

Module `module/spring-boot-micrometer-metrics`, class
`export.otlp.OtlpMetricsPropertiesConfigAdapterTests`: HANG, no JUnit result
ever produced (empty `.out.log`, process killed at shard timeout).

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-b363ecc808ba.err.log`

The `.err.log` shows normal VM startup (post-clinit fixups), then a
`Mockito is currently self-attaching to enable the inline-mock-maker` warning
(this class uses `org.mockito.BDDMockito.given`/`org.mockito.Mockito.spy`),
then a burst of the benign, already-tracked
`gen_heap::get_field: out-of-bounds field read dropped` warnings against
`org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`
(`num_slots=0`) — and finally, as the **last line in the entire log** before
the process goes silent for the rest of the shard timeout:

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped ... class_name=net/bytebuddy/utility/Invoker$Dispatcher real_field_count=Some(0)
```

No further log output of any kind follows — unlike the
`InterceptingExecutableInvoker` OOB warnings (confirmed elsewhere as benign,
recurring noise during normal JUnit bootstrap), this is the only occurrence
of `net/bytebuddy/utility/Invoker$Dispatcher` in the log, and nothing follows
it. This is a different symptom shape from the two Log4j2/Logback
`ModifiedClassPathExtension` HANGs in this module (see
[`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md)) — this class does
not use `@ClassPathExclusions`/`@ClassPathOverrides`/`ModifiedClassPathExtension`
at all, only plain Mockito mocking (`BDDMockito.given`, `Mockito.spy`) on a
config-adapter POJO. Not clustered with that HANG group.

## Root cause

**Not confirmed — no debugger/thread-dump was attached to a live hung
process this session.** The last-observed activity (`net/bytebuddy/utility/Invoker$Dispatcher`,
part of ByteBuddy's mechanism for invoking JDK-internal methods reflectively
when installing Mockito's inline-mock-maker Java agent via self-attach) is
the strongest lead: Mockito's self-attach path
(`ByteBuddyAgent.install()` → `VirtualMachine.attach(pid)` →
`loadAgent`/`Instrumentation.redefineClasses`) is a heavyweight,
JVM-internals-touching operation that real HotSpot supports natively. If
CratonVM's `com.sun.tools.attach`/`java.lang.instrument` support is
incomplete or blocks on a resource that's never satisfied (e.g. a
self-attach loop retrying against a `VirtualMachineDescriptor` that never
resolves, or an `Instrumentation` native handshake that never completes),
that would explain both the silent hang and why it starts exactly at
ByteBuddy's dispatcher-invocation step. Not confirmed against CratonVM
source this session — needs a live thread dump (`cdb`/`gdb` attach mid-hang,
per [[reference_crash_debug_tooling]] in project memory) to identify which
thread/lock is actually stuck before further hypothesizing.

## Update 2026-07-17 (bin11 rerun triage) — 1 more class, slightly different last-line detail

`module/spring-boot-jackson`'s
`org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests`
HANGs with a closely related shape: `.out.log` empty, `.err.log` (518
lines) shows normal startup, a ~20s burst of the benign
`gen_heap::get_field: out-of-bounds field read dropped` warning against many
*different*, rapidly-changing `org/springframework/core/$Proxy34` object
addresses (489 occurrences — ordinary bean-creation/proxy churn, same
"unrelated burst" shape this doc's sibling
`mockwebenvironmentservletcomponentscanintegrationtests-hang.md` already
identified as not-the-hang-itself), and then, as the **literal last line in
the entire log**:

```
Mockito is currently self-attaching to enable the inline-mock-maker. This will no longer work in future releases of the JDK. Please add Mockito as an agent to your build as described in Mockito's documentation: https://javadoc.io/doc/org.mockito/mockito-core/latest/org.mockito/org/mockito/Mockito.html#0.3
```

— nothing follows, ever (process killed at shard timeout). This is one step
*earlier* in the same sequence than this doc's original symptom (which
stops one line later, at the `net/bytebuddy/utility/Invoker$Dispatcher` OOB
warning that follows Mockito's self-attach message) — here the process
doesn't even get as far as that ByteBuddy dispatcher probe before going
silent. Consistent with the same underlying hypothesis (Mockito's
`ByteBuddyAgent.install()` self-attach path stalling under CratonVM), just
caught at an earlier point in the sequence; not independently confirmed
against CratonVM source this session either. Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jackson.org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests.err.log`

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.OtlpMetricsPropertiesConfigAdapterTests` |
| `module/spring-boot-jackson` | `org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests` (added bin11) |
