# Observability

Observability in CratonVM is layered: application telemetry explains what the
Java program is doing, while VM diagnostics explain class loading, compilation,
allocation, collection, safepoints, and native bridges. Capture both layers for
an actionable incident.

## Signals to collect

For a long-running service, collect at least:

| Signal | Why it matters |
|--------|----------------|
| Process exit status and restart count | Distinguishes application failure from supervisor churn. |
| Resident memory and cgroup memory events | Separates Java-heap pressure from native-memory or container eviction. |
| CPU time, run queue, and throttling | Prevents host contention from being misread as a VM regression. |
| Application request/error/latency metrics | Establishes user-visible impact. |
| GC and safepoint diagnostics | Explains pauses and allocation pressure. |
| JIT compilation/fallback diagnostics | Explains interpreter-heavy or unstable compiled execution. |
| Class-loading errors and native audit output | Explains compatibility failures. |

CratonVM does not expose a single mandatory metrics endpoint. Use the
application's metrics system for service-level telemetry and OS/container
instrumentation for process-level telemetry.

## Built-in diagnostic surfaces

### Class and GC logging

```bash
cratonvm --verbose:class ...
cratonvm --verbose:gc ...
cratonvm --Xlog "gc*=info:stdout:time,level,tags" ...
```

Enable only the categories needed for the investigation. Class-load and
fine-grained GC logging can be high volume.

### Java Flight Recorder

CratonVM includes a JFR implementation for event-oriented recording. Use it to
correlate allocation, collection, method, and thread activity over time. Treat
recordings as potentially sensitive: stack traces, class names, file paths,
and application values can reveal internal data. See
[Profiling](../performance/profiling.md).

### Timeout stack dumps

For a suspected hang:

```bash
cratonvm --stack-dump-on-timeout 30 ...
```

The timeout captures interpreter thread stacks before aborting. Set a duration
long enough to avoid killing a merely slow startup. The launcher also has a
default watchdog; see [Debugging and Diagnostics](../user-guide/debugging.md)
for its controls.

### Native coverage audit

When real JDK bytecode reaches an unsupported native entry point:

```bash
cratonvm --XX:AuditMissingNatives ...
cratonvm --dump-missing-natives missing.json ...
cratonvm --dump-missing-natives-grouped missing-by-module.json ...
cratonvm --dump-native-registry registry.json ...
```

The missing-native outputs answer "what did this workload actually request?"
The complete registry answers "what could this binary provide?" Preserve both
when reporting a compatibility gap.

### JIT isolation

Run the same deterministic reproduction with and without compilation:

```bash
cratonvm ...
cratonvm --nojit ...
```

If only the JIT run fails, preserve the exact class files, arguments, JDK, and
both outputs. If both fail identically, start with interpreter, class-loading,
native, or application semantics rather than assuming a compiler bug.

## An incident evidence bundle

Collect a small, bounded directory:

```text
incident/
  command.txt          exact argv with secrets redacted
  environment.txt      relevant CRATONVM_* names and non-secret values
  versions.txt         CratonVM commit/version, JDK, OS, architecture
  stdout.log
  stderr.log
  process.txt          CPU/RSS/cgroup snapshot and host load
  jit.out              deterministic JIT run
  nojit.out            matching --nojit run
  missing-natives.json optional native audit
  recording.jfr        optional, access-controlled
  reproducer/          minimal source/classes and invocation
```

Do not dump the entire environment: it often contains credentials. Record only
configuration that affects the VM, and redact secrets in `-D` properties and
application arguments.

## Reading common patterns

| Observation | Likely next question |
|-------------|----------------------|
| JIT fails, `--nojit` succeeds | Which method compiled, and is the output deterministic? |
| Both modes fail during bootstrap | Is the selected JDK complete and is `--java-home` correct? |
| RSS reaches the cgroup limit below `-Xmx` | Was native-memory headroom omitted? |
| Frequent young collections with stable live set | Is the heap too small or allocation rate unexpectedly high? |
| Long pauses only with moving young or G1 | Does the default collector remove the symptom? |
| Missing native appears | Does the real JDK class genuinely declare it native, and which module owns it? |
| High wall time with low process CPU | Is the process blocked on I/O, a monitor, or host scheduling? |
| High wall time and high system load | Repeat in a quiet window before asserting a regression. |

## Retention and privacy

Set retention limits for verbose logs and JFR recordings. Avoid collecting
class bytes or heap-like data from customer workloads unless the incident
requires it and access is controlled. Bug reports should prefer a minimal
synthetic reproduction over production data.
