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

`--dump-native-registry` writes `"schema_version": 2`. Alongside the
intrinsic/bridge/synthetic-stub kind, every row carries the `register()` call
site (`registered_by`), the kind that registration replaced in place
(`overwrote`) and this run's dispatch count (`invocations` — dispatches, not
registrations, and a lower bound because some warm and compiled dispatch paths
carry no slot handle to count from). `real_declaring_method` is present but is
currently always `null`. Rows are sorted for byte-stable output, so the file
diffs cleanly against a committed baseline. See [Native
Methods](../internals/native-methods.md#the-native-registry-census-schema-2).

### Compatibility-substitution census

The native audit says what the VM can provide. A second family says what it had
to *substitute* — classes fabricated without real bytes, and synthetic-stub
natives registered or dispatched:

```bash
cratonvm --dump-class-origins origins.json ...
cratonvm --jdk-only-report jdk-only.json ...
cratonvm --trace-jdk-only --explain-jdk-only ...
```

None of these require `--jdk-only`; under the default policy they record what a
strict run *would* have rejected, which makes them a measurement rather than a
post-mortem. Both files are written even when the run fails, and both are
sorted so they can be committed as a baseline and diffed release over release.
`--trace-jdk-only` polls the violation logs after VM construction and at
shutdown rather than hooking each recording site, so mid-run class-origin
violations appear at shutdown.

Keep `--explain-jdk-only` **off** for any artifact leaving the host: it
disables the path redaction that otherwise rewrites absolute filesystem paths
in all three report files. See [JDK-Only
Mode](../user-guide/jdk-only-mode.md).

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
  origins.json         optional class-origin census
  jdk-only.json        optional compatibility-substitution report
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
