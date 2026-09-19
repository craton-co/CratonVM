# Debugging & Diagnostics

CratonVM provides several facilities for debugging Java programs running on it,
inspecting what the VM is doing, and finding the cause of a hang or a missing
feature.

## Remote debugging (JDWP)

CratonVM can expose a JDWP debug server so you can attach a Java debugger (IDE
or `jdb`):

```bash
# Start a debug server on port 5005
cratonvm --jdwp-port 5005 --classpath . MyApp

# Suspend at startup until a debugger attaches
cratonvm --jdwp-port 5005 --jdwp-suspend --classpath . MyApp
```

`--jdwp-port` is equivalent to the HotSpot
`-agentlib:jdwp=transport=dt_socket,server=y,address=<port>` form. Point your
IDE's "remote JVM debug" configuration at that port.

## Agents

| Flag | Purpose |
|------|---------|
| `-agentlib:<spec>` | Load a native JVMTI agent from the standard library path. |
| `-agentpath:<path>[=opts]` | Load a native JVMTI agent from an absolute path. |
| `-javaagent:<jar>[=opts]` | Load a Java agent JAR with a `Premain-Class` manifest header. |

## Verbose tracing

| Flag | Shows |
|------|-------|
| `--verbose:class` | Each class as it is loaded. |
| `--verbose:gc` | Garbage-collection activity. |
| `--Xlog <spec>` | HotSpot-style unified logging, e.g. `--Xlog "gc*=info:stdout:time,level,tags"`. |

## Diagnosing a hang

If a program appears frozen, CratonVM can dump every interpreter thread's stack
and abort, so you can see where it is stuck:

```bash
cratonvm --stack-dump-on-timeout 30 --classpath . MyApp
```

A 120-second watchdog is installed automatically; pass `0` to
`--stack-dump-on-timeout`, or set `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, to opt
out. Adjust the automatic default with `CRATONVM_DEFAULT_WATCHDOG_SEC`.

A common cause of an apparent hang is JIT compilation of a pathological method —
confirm by re-running with `--nojit` (see [The JIT
Compiler](jit-compiler.md)).

## Finding missing standard-library methods

CratonVM implements a large subset of the standard library. When your program
calls a `native` method that CratonVM hasn't implemented, you'll typically see
an error. To audit what's missing across a whole run:

```bash
# Log every unimplemented native invoked, printed on shutdown
cratonvm --XX:AuditMissingNatives --classpath . MyApp

# Dump the audit to JSON
cratonvm --dump-missing-natives missing.json --classpath . MyApp

# Same, grouped by JDK module
cratonvm --dump-missing-natives-grouped missing-by-module.json --classpath . MyApp
```

To see everything the VM *does* register (classified as intrinsic, bridge, or
synthetic-stub):

```bash
cratonvm --dump-native-registry natives.json --classpath . MyApp
```

That file is `"schema_version": 2`. Each row carries, beyond the kind, the
`register()` call site (`registered_by`), the kind this registration replaced
in place (`overwrote` — registrations overwrite by triple, last write wins),
and this run's dispatch count (`invocations`, so `0` means the entry cost this
workload nothing). `real_declaring_method` is present but currently always
`null`. The field-by-field description is in [Native
Methods](../internals/native-methods.md#the-native-registry-census-schema-2).

These audits are the fastest way to understand why a particular library doesn't
work yet and to file an actionable bug report.

## Auditing compatibility substitutions

The three flags above say what the VM *provides*. A separate family says what
the VM had to *substitute* — classes it fabricated with no real bytes, and
synthetic-stub natives it registered or dispatched:

```bash
# What did this run have to fabricate, and what would strict mode reject?
cratonvm --dump-class-origins origins.json \
         --jdk-only-report jdk-only.json \
         --classpath . MyApp

# Same run, with each violation reported to stderr and explained in full.
cratonvm --trace-jdk-only --explain-jdk-only --classpath . MyApp
```

None of these require `--jdk-only`. Under the default compatibility policy the
violations recorded are the ones a strict run *would* have hit, so the files
measure the distance to strict mode before anything is enforced.

`--trace-jdk-only` polls rather than hooks: the launcher drains the violation
logs after VM construction and again at shutdown, so a class-origin violation
recorded mid-run is reported at shutdown. `--explain-jdk-only` additionally
turns off path redaction in all three report files — leave it off when the
output is going into a bug report or a committed baseline.

See [JDK-Only Mode](jdk-only-mode.md) for the origin vocabulary and for what
`--jdk-only` itself changes.

## Java Flight Recorder (JFR)

CratonVM includes a JFR implementation for event-based profiling and
diagnostics. See [Profiling](../performance/profiling.md).

## Detailed NPE messages

Helpful `NullPointerException` messages (JEP 358) are on by default; toggle with
`--XX:ShowCodeDetailsInExceptionMessages` (the HotSpot
`-XX:+`/`-XX:-ShowCodeDetailsInExceptionMessages` spellings work too). Turning it
off matches HotSpot exactly: `getMessage()` returns `null` for an NPE the VM
raised from a null dereference. Messages that library code passed explicitly —
`Objects.requireNonNull(x, "…")` — are unaffected either way.

## Filing a useful bug report

Include:

1. A minimal Java reproduction (source and, ideally, the `.class`).
2. The exact command line you ran.
3. Expected vs. actual output.
4. Whether it reproduces under `--nojit` and/or `--synthetic-jdk`.
5. `cratonvm --version`, your OS, and Rust version.
6. For a missing, wrong or apparently fabricated standard-library class, the
   `--dump-class-origins` and `--jdk-only-report` files (without
   `--explain-jdk-only`, so paths stay redacted).
