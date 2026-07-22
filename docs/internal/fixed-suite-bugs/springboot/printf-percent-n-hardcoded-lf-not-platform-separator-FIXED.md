# `String.format`/`PrintStream.printf`'s `%n` conversion hardcodes `\n` instead of `System.lineSeparator()` (breaks on Windows)

**Status: FIXED, verified 2026-07-17 — commit `0d90dd6f3`.** Rebuilt
(`cargo build --release`) and reran `HelpCommandTests` (2/2) and
`ToolsJarModeTests` (9/9, `9 tests`) against the fresh binary: both now
**PASS**. The other classes listed below (`core/spring-boot`'s 6
structured-log-formatter classes, `ChangelogWriterTests`) were not
individually reverified this round but share the exact same root-cause
mechanism (all three call surfaces — `printf`, `String.format`,
`String.formatted` — converge on the single `native_string_format`
function this fix patches) — very likely also fixed, flagged for
confirmation on the next full rerun rather than re-investigated.

## Symptom

| Class | Failures |
|---|---|
| `loader/spring-boot-jarmode-tools` `HelpCommandTests` | 2 of 2 |
| `loader/spring-boot-jarmode-tools` `ToolsJarModeTests` | 6 of 6 |

Every failure is an `AssertJ`/`opentest4j` `AssertionFailedError` from
`TestPrintStream$PrintStreamAssert.hasSameContentAsResource`, comparing
captured command output against a classpath text-fixture resource. The
printed "expected" and "actual" strings render as **visually identical** in
the JUnit console dump (both wrap to the same lines), which is what made
this easy to misdiagnose as a flaky/non-issue on first read — the actual
difference is invisible at the text level:

```
=> org.opentest4j.AssertionFailedError:
Expecting actual's toString() to return:
  "Usage:
  java -Djarmode=tools -jar test.jar

Available commands:
  test  Description of test
  help  Help about any command
"
but was:
  "Usage:
  java -Djarmode=tools -jar test.jar

Available commands:
  test  Description of test
  help  Help about any command
"
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-jarmode-tools.org.springframework.boot.jarmode.tools.HelpCommandTests.out.log`

A byte-level dump of the log file shows the mechanism precisely: the
`"Usage:\r\n"` / `"java -Djarmode=...\r\n\r\n"` / `"Available commands:\r\n"`
lines (each produced by `PrintStream.println(...)`) all end in `\r\n`
(`0x0D 0x0A`) — but the **command-summary table rows**
(`"  test  Description of test"` / `"  help  Help about any command"`,
each produced by `printCommandSummary`/`printOptionSummary` — see below) end
in a **bare `\n`** (`0x0A` only) on the CratonVM ("actual") side, while the
classpath fixture resource ("expected") has `\r\n` there too, matching
Windows' `System.lineSeparator()`.

## Root cause (CONFIRMED)

`HelpCommand` (`apps/spring-boot/loader/spring-boot-jarmode-tools/src/main/java/org/springframework/boot/jarmode/tools/HelpCommand.java`)
prints most lines via `PrintStream.println(...)` (correct, platform-aware),
but prints the command/option summary table rows via `printf`'s `%n`
conversion:

```java
// HelpCommand.java:128-130
private void printCommandSummary(PrintStream out, Command command, int padding) {
    out.printf("  %-" + padding + "s  %s%n", command.getName(), command.getDescription());
}
// HelpCommand.java:91-93
private void printOptionSummary(PrintStream out, Option option, int padding) {
    out.printf("  --%-" + padding + "s  %s%n", option.getNameAndValueDescription(), option.getDescription());
}
```

`PrintStream.printf`/`format` is natively overridden in CratonVM
(`native-builtins/src/lib.rs`, `native_printf`, registered on
`java/io/PrintStream` for both `printf` and `format`), which delegates to
`String.format`'s native implementation,
`native_string_format` (`native-builtins/src/lang_string.rs:4471`). Its
format-string parser hardcodes the `%n` conversion to a literal `'\n'`
instead of the platform line separator:

```rust
// native-builtins/src/lang_string.rs:4526-4530
if chars[i] == 'n' {
    result.push('\n');
    i += 1;
    continue;
}
```

Real `java.util.Formatter`'s `%n` conversion is defined to emit
`System.lineSeparator()` — `"\r\n"` on Windows, matching what
`PrintStream.println()` already correctly does elsewhere in CratonVM (it
uses a different, correct native path that reads the platform separator).
There is even a source comment elsewhere in the same file
(`native-builtins/src/lib.rs:28788-28793`, next to `System.initPhase1`'s
registration) stating the *intent* — `"The full init also drops
'lineSeparator' into place so String.format("%n") matches HotSpot"` — but
the actual conversion code at `lang_string.rs:4526` never consults that
value; it always substitutes a bare `\n` regardless of platform.

An identical, second hardcoded copy of the same bug exists in a sibling
(currently dead/uncalled) formatter helper,
`simple_java_format` (`native-builtins/src/lib.rs:81519`, the `'n' => {
result.push('\n'); }` arm at line 81571-81573) — not on the live call path
for `printf`/`String.format` today (nothing in `lib.rs` calls
`simple_java_format`), but worth fixing at the same time so it doesn't
reintroduce the same bug if it's ever wired up.

## Impact

This is **not** jarmode-tools-specific — `%n` is a standard, extremely
common `Formatter` conversion used throughout the JDK and every framework on
the classpath (logging, `String.format`, `Formatter`, `PrintWriter.printf`,
etc.) any time code explicitly asks for the platform line separator instead
of a literal `\n`. On Windows this diverges from HotSpot every time; it only
surfaces as a *test failure* here because these two classes happen to
byte-compare formatted output against a fixture resource, but the underlying
defect has much broader (silent) blast radius on Windows builds — any
consumer that writes `%n`-formatted text to a file/stream and expects `\r\n`
will silently get `\n` instead.

## Fix direction

In `native_string_format` (`native-builtins/src/lang_string.rs:4526`),
replace the hardcoded `result.push('\n')` with the real platform line
separator — the same value `System.lineSeparator()`/`PrintStream.println()`
already source correctly elsewhere in this VM (see
`native-builtins/src/lang_system.rs:1125`,
`native_system_line_separator`). Apply the identical fix to the dead
`simple_java_format` arm (`native-builtins/src/lib.rs:81571`) for
consistency/future-proofing.

## Update 2026-07-17 (large `core/spring-boot` batch triage, 73-class rerun doc pass) — same bug explains a 6-class structured-logging cluster; fix already landed in this worktree

6 more classes, `core/spring-boot`'s `log4j2`/`logback` structured log
formatter tests, fail `shouldFormatException()` with the exact same
underlying mechanism (confirmed independently, before cross-referencing
this doc):

| Module | Class |
|---|---|
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.ElasticCommonSchemaStructuredLogFormatterTests` |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.GraylogExtendedLogFormatStructuredLogFormatterTests` |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.LogstashStructuredLogFormatterTests` |
| `core/spring-boot` | `org.springframework.boot.logging.logback.ElasticCommonSchemaStructuredLogFormatterTests` |
| `core/spring-boot` | `org.springframework.boot.logging.logback.GraylogExtendedLogFormatStructuredLogFormatterTests` |
| `core/spring-boot` | `org.springframework.boot.logging.logback.LogstashStructuredLogFormatterTests` |

```
JUnit Jupiter:ElasticCommonSchemaStructuredLogFormatterTests:shouldFormatException()
  => java.lang.AssertionError:
Expecting actual:
  "java.lang.RuntimeException: Boom
	at org.springframework.boot.logging.log4j2.ElasticCommonSchemaStructuredLogFormatterTests.shouldFormatException(ElasticCommonSchemaStructuredLogFormatterTests.java:95)
	at org.junit.platform.commons.util.ReflectionUtils.invokeMethod(ReflectionUtils.java:701)
	... (full raw stack trace)
"
to start with:
  "java.lang.RuntimeException: Boom
	at org.springframework.boot.logging.log4j2.ElasticCommonSchemaStructuredLogFormatterTests.shouldFormatException"
     org.springframework.boot.logging.log4j2.ElasticCommonSchemaStructuredLogFormatterTests.shouldFormatException(ElasticCommonSchemaStructuredLogFormatterTests.java:106)
```

The relevant test asserts `.startsWith(String.format("java.lang.RuntimeException: Boom%n\tat ..."))`
(`ElasticCommonSchemaStructuredLogFormatterTests.java:106-107`) — that
`String.format(...%n...)` call is exactly this doc's bug: on this
`craton-rerun-20260717` binary, `%n` produced a bare `\n` instead of
Windows' `\r\n`, so the *expected* prefix string built by the test itself
diverges from CratonVM's actual `Throwable.printStackTrace()` output right
after `"Boom"` (the `printStackTrace`/`println` path used to produce
`actual` is unaffected — it already emits real `\r\n` via a different,
correct native code path, which is exactly why the two sides don't just
agree on the same wrong separator). Formatter's default fallback (no
explicit `StackTracePrinter`) is `Extractor.printStackTrace()`
(`apps/spring-boot/core/spring-boot/src/main/java/org/springframework/boot/logging/log4j2/Extractor.java:59-63`,
identical in the `logback` package) — `throwable.printStackTrace(new
PrintWriter(stringWriter))` — confirming the only `%n`-affected side is the
test's own expected-value construction.

**This worktree's current `HEAD` (`0d90dd6f3`, this doc's own fix commit)
already contains the fix** — `native-builtins/src/lang_string.rs`'s `%n`
handling now reads:

```rust
if chars[i] == 'n' {
    result.push_str(if cfg!(windows) { "\r\n" } else { "\n" });
    i += 1;
    continue;
}
```

`git log` shows `0d90dd6f3` was committed 2026-07-17 19:50:26 -0300
(≈22:50 UTC), while this `core/spring-boot` rerun's logs are timestamped
~20:44 UTC the same day — i.e. the binary that produced these 6 failures
predates the fix. Per this session's constraints (no rebuild/rerun), this
was not re-verified against a fresh build, but given the mechanism is
identical to this doc's already-confirmed root cause and the fix is
already present in source, these 6 classes are very likely resolved by the
same fix and should be re-verified (not re-investigated) on the next
rebuild.

Full logs (`craton-rerun-20260717/shard1/logs/core_spring-boot.<class>.out.log`
for each class above).

## Update 2026-07-17 (bin7 rerun triage) — `ChangelogWriterTests`, same mechanism via `String.formatted()` this time

`configuration-metadata/spring-boot-configuration-metadata-changelog-generator`'s
`ChangelogWriterTests.writeChangelog()` fails byte-comparing generated
asciidoc changelog output against a checked-in fixture
(`src/test/resources/sample.adoc`):

```
=> org.opentest4j.AssertionFailedError:
Expecting actual's toString() to return:
  "Configuration property changes between `1.0` and `2.0`
...
```
Visually identical expected/actual in the console dump, same trap as the
`HelpCommandTests` symptom above. A byte-level diff (`expected` from the
`.adoc` fixture vs. `actual` from `ChangelogWriter`'s output) pinpoints the
exact same divergence: expected has `\r\n\r\n\r\n\r\n` where actual has
`\n\n\n\n` at every blank-line run between changelog sections.

`ChangelogWriter.write(Changelog)` (`ChangelogWriter.java:70-218`) uses
`%n` exclusively for every line break (`write("...%n", ...)`, never
`println`), routed through a private `write(String format, Object...
strings)` helper (`ChangelogWriter.java:229`) that calls
`this.out.append(format.formatted(strings))` — i.e. `String.formatted`,
not `String.format`. This confirms the bug is not narrowly a
`PrintStream.printf`/`String.format` issue: `String.formatted(Object...)`
(`native-builtins/src/lang_string.rs:5241`, `native_string_formatted`) is
a thin wrapper that itself calls `native_string_format` — the exact same
function this doc's fix patches — so this is the identical bug via a third
call surface (`printf`, `String.format`, and now `String.formatted`, all
converging on one native function).

**Same "predates the fix" situation as the `core/spring-boot` batch above:**
this test's log is timestamped `2026-07-17T19:52:00Z`, i.e. within ~2
minutes of the fix commit (`0d90dd6f3`, 19:50:26 -03:00 = 22:50:26 UTC —
note the log timestamp is *earlier* in UTC than the commit's UTC time,
consistent with the binary that ran this test having been built before the
fix landed). Per this session's constraints (no rebuild/rerun performed),
not re-verified against a fresh build — but the mechanism is identical to
this doc's confirmed root cause and the fix already covers the exact code
path (`native_string_formatted` → `native_string_format`), so this class
is very likely already resolved by `0d90dd6f3` and should be re-verified
(not re-investigated) on the next rebuild+rerun rather than treated as a
separate open bug.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/configuration-metadata_spring-boot-configuration-metadata-changelog-generator.org.springframew-ef7b0d85d822.out.log`

## Affected classes

| Module | Class |
|---|---|
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.HelpCommandTests` |
| `loader/spring-boot-jarmode-tools` | `org.springframework.boot.jarmode.tools.ToolsJarModeTests` |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.ElasticCommonSchemaStructuredLogFormatterTests` (added large-batch triage, likely already fixed by `0d90dd6f3` — see update above) |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.GraylogExtendedLogFormatStructuredLogFormatterTests` (same) |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.LogstashStructuredLogFormatterTests` (same) |
| `core/spring-boot` | `org.springframework.boot.logging.logback.ElasticCommonSchemaStructuredLogFormatterTests` (same) |
| `core/spring-boot` | `org.springframework.boot.logging.logback.GraylogExtendedLogFormatStructuredLogFormatterTests` (same) |
| `core/spring-boot` | `org.springframework.boot.logging.logback.LogstashStructuredLogFormatterTests` (same) |
| `configuration-metadata/spring-boot-configuration-metadata-changelog-generator` | `org.springframework.boot.configurationmetadata.changelog.ChangelogWriterTests` (added bin7, via `String.formatted()` — likely already fixed by `0d90dd6f3`, see update above) |
