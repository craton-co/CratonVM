# Real `java.util.logging.SimpleFormatter.format()`'s own bytecode drops the date (`%1$tc`) and message (`%5$s`) `String.format` args — even with a known-good pattern string

**Status: OPEN — found 2026-07-24.** Residual of the Spring Boot core Cluster C
(logging bootstrap) repair batch. Blocks
`org.springframework.boot.logging.java.JavaLoggingSystemTests#testNonDefaultConfigLocation`
only; 11/12 methods in that class now pass (was 3/12 before this batch — see
the JUL fixes in `native-builtins/src/logmanager.rs`,
`native-builtins/src/lib.rs`, and the removed stale `ConsoleHandler` stub in
`native-builtins/src/phases_early.rs`).

## Symptom

`testNonDefaultConfigLocation` supplies a minimal inline
`logging-nondefault.properties` (`handlers = java.util.logging.ConsoleHandler`
/ `.level = INFO`, no `.formatter` key), relying on `ConsoleHandler`'s own
default `java.util.logging.SimpleFormatter`. Expected output contains
`"INFO: Hello world"`; actual output is just `"INFO: \n"` — both the date
(`%1$tc`) and the message (`%5$s`) are empty.

## What's ruled out

- `LogRecord.getMessage()` called directly returns the correct text.
- `Formatter.formatMessage(LogRecord)` (native override in
  `logmanager.rs`, reads via `getMessage()`) called directly (reflection or
  plain `invokevirtual`) returns the correct text.
- `String.format("%1$tc%n%4$s: %5$s%n", zdt, ..., message, ...)` called
  directly, with the exact same argument types (a real `ZonedDateTime`, a
  computed `String`), produces fully correct output.
- Building an `Object[]` array from two method-call results (mimicking
  `SimpleFormatter.format()`'s own `anewarray`/`aastore` sequence for its
  varargs call) and passing it to `String.format` also works correctly.
- Not a null-format-string issue: `apply_jul_config_entries`
  (`logmanager.rs`) now unconditionally overwrites the private `format`
  field on every handler-less-configured `SimpleFormatter` with a known-good
  literal pattern before `setFormatter` — the output is unchanged. The `%4$s`
  (level) slot resolves correctly in the same call, ruling out the pattern
  string itself.
- Not specific to CratonVM's own `ConsoleHandler`/`Handler` stub layer (that
  stub was removed this session): the same `"INFO: \n"` truncation appears
  calling `SimpleFormatter.format(record)` directly, with no `Handler`
  involved at all.

## Likely root cause (unconfirmed)

Only the REAL, javac-compiled `java.util.logging.SimpleFormatter.format()`
method reproduces this — every hand-written Java equivalent of the same
logic (verified via `javap -c` against the real method) works. The real
method's bytecode stores the computed `ZonedDateTime` and the
`formatMessage()` result into local variable slots using the wide
`astore <n>`/`aload <n>` form (slots 2 and 4 respectively, since the method
has more than 4 locals) before loading them into the `Object[]` args array
for the `String.format` varargs call. A hand-rolled equivalent with fewer
locals gets compiled with different (lower) slot numbers and does not
reproduce the bug. Suspect an interpreter/JIT correctness gap specific to
wide-form local variable slot access feeding directly into an `anewarray`/
`aastore` sequence, but this has not been isolated to a minimal repro yet —
several attempts to reproduce with an explicit `Object[]` array populated
from method-call results (including with padding locals to force wide
slot indices) did not reproduce it.

**Possibly related:** two other real-bytecode-only state-loss bugs found
in the same session, both in methods with an actual exception table
(try/catch or try/finally) — see
`exception-table-method-state-loss-cluster.md`. This method's failing call
path has no exception table itself, so the shared mechanism (if any) is
unconfirmed, but the overall shape — real bytecode silently losing
correctly-computed state that every hand-replicated equivalent preserves —
is the same.

## Why this is scoped OPEN rather than fixed

Spring Boot's own `JavaLoggingSystem` (and every other Cluster C test)
always sets an explicit formatter — real, unconfigured
`java.util.logging.SimpleFormatter` is a narrow fallback path Spring Boot
itself doesn't rely on in normal operation. Chasing the underlying
interpreter/JIT bug further is out of scope for the logging-bootstrap batch;
flagging here so a VM-level investigation can pick it up with a fresh,
minimal repro strategy (e.g. bisecting local-slot count in a hand-written
class to find the wide-vs-compact `astore` threshold that reproduces it).

## Repro

```powershell
$env:JAVA_HOME = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
& <cratonvm.exe> -cp . SimpleFormatterRepro
```
```java
import java.util.logging.*;
public class SimpleFormatterRepro {
    public static void main(String[] args) throws Exception {
        LogRecord r = new LogRecord(Level.INFO, "HELLO");
        SimpleFormatter f = new SimpleFormatter();
        System.err.println("[" + f.format(r) + "]"); // expect date+"INFO: HELLO", get "INFO: \n"
    }
}
```
