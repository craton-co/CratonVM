# A `-javaagent` transformer is accepted and never called, and `VirtualMachine.list()` throws

| | |
|---|---|
| **Status** | OPEN — reproduced, not diagnosed |
| **Severity** | medium-high — `addTransformer` succeeding and doing nothing is the worst shape a failure can take |
| **Modes** | BOTH. `--real-jdk` and `--jdk-only` are identical |
| **Opened** | 2026-08-05, by `probes/JdkOnlyPlatformProbe`'s `agent` section (L8, criterion 6) |

## What happens

`probes/JdkOnlyProbeAgent` is a real agent jar: `Premain-Class` manifest,
passed as `-javaagent:` to all three arms of
`scripts/jdk-only-strict-probes.sh`. Its `premain` registers a
`ClassFileTransformer` that counts every class it is offered and records
whether it saw the probe's own main class being defined.

| | HotSpot 25 | CratonVM |
|---|---|---|
| `premainRan()` | `true` | `true` |
| `isRetransformClassesSupported()` | `true` | `true` |
| classes offered to the transformer | `positive` | **`zero`** |
| transformer saw `JdkOnlyPlatformProbe` being defined | `true` | **`false`** |
| `Instrumentation` interface method count | `17` | `17` |
| `com.sun.tools.attach.VirtualMachine.list()` | returns a `List` | **throws `InternalError`** |

## Why each row matters

**The agent launches.** `premain` runs, on the launch thread, before `main` —
the jar is opened, the manifest is read, the agent class is defined by the
system loader and the entry point is invoked with a live `Instrumentation`.
Everything up to and including the handshake works.

**`addTransformer` is then a no-op.** It accepts the transformer without
throwing and the transformer is never called, not once, for any class — while
`isRetransformClassesSupported()` answers `true`. An agent has no way to
discover this: the API contract is "you will be offered every subsequent
definition", and CratonVM's answer to "can you retransform?" is yes.

That is worse than an `UnsupportedOperationException` would be. Every
bytecode-rewriting agent — coverage (JaCoCo), APM, mocking, tracing, most
profilers — reports that it installed successfully and then silently
instruments nothing. A suite run under such an agent produces plausible,
entirely fictional output.

**`selfSeen=false` localizes it.** The probe's own main class is defined
*after* `premain` returns, so a transformer on the live definition path would
see it. It does not, so the gap is on the definition path, not a
registered-too-late race.

**`VirtualMachine.list()` throwing `InternalError` is a separate row.** The
class resolves (`jdk.attach` is present), so this is not a missing module; the
call itself fails. HotSpot returns a list — possibly empty, which is why the
probe asserts only that a `List` came back and never its size. `InternalError`
is not in that method's contract at all, so a caller cannot handle it.

## Where to start

`-javaagent:` parsing and `premain` invocation are in `vm-cli/src/main.rs`
(`parse_javaagent_spec`, and the premain loop around line 4072). Since premain
demonstrably runs with a live `Instrumentation`, the question is what that
object's `addTransformer` does with what it is handed, and whether the class
definition path consults a transformer list at all. The probe's `agent` line
is the regression test: `transformed=positive selfSeen=true` is the target,
and it is asserted against HotSpot rather than against a literal, so it cannot
freeze a wrong answer.
