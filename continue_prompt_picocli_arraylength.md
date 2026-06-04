# Bug: `arraylength` executed on a java.util.ArrayList (picocli getTerminalWidth)

Cosmetic / non-fatal (the guard returns 0 and execution continues), but it pollutes
stderr and zeroes the console terminal width → empty console help/summary (XML reports
still write fine). Independent of the other `continue_prompt_*` bugs.

## Symptom
A native calls the `array_length` helper (`vm/src/vm/vm_exec.rs:1409`) on a
`java.util.ArrayList`, caught by `[ARRAY-LEN-GUARD] non-array object class=java/util/ArrayList
... caller=...picocli.CommandLine$Model$UsageMessageSpec$1.run()V pc=94`. pc=94 is the
`astore_1` right after `ProcessBuilder.start()` in picocli's `getTerminalWidth()`.

## Ruled out this session
- It is NOT either `ProcessBuilder.start` native:
  - `native_pb_start` (`native-builtins/src/lang_system.rs`) is **dead** (`PB-START-OLD`
    never prints; it's overridden). It was hardened anyway (now reads the command List by
    field name; `array_length` only on a genuine array) — committed, but NOT the trigger.
  - `phases_late.rs::register_phase57_process` `start` (`PB-START-ENTRY`) wasn't even
    called on this path AND already handles Lists by name.
- So the `array_length` comes from some OTHER native on the `getTerminalWidth` path — the
  reflective `redirectError`/`Class.forName`/`getDeclaredMethod`/`Method.invoke` dance, or a
  real-bytecode `ProcessImpl`/env conversion.

## The blocker to pinning it
The guard prints a Rust backtrace to name the source native, but it is **unsymbolizable
garbage** — a recursive `core::net::socket_addr::impl$6::fmt` chain (the
`std::backtrace::Backtrace::force_capture` symbolizer is itself broken on this build).

## Next steps
1. Fix the broken backtrace symbolization, OR (simpler) tag the `array_length` helper guard
   with the calling native's identity directly — e.g. thread a `&'static str` native name
   through, or capture+log the immediate Rust caller via `#[track_caller]` /
   `std::panic::Location`, instead of `Backtrace::force_capture`.
2. Re-run the repro; the offending native is then obvious. Fix it to read the List by name
   (mirror the `phase57` pattern) or to only call `array_length` on a genuine array
   (`heap_kind_of(obj) == ObjectKind::Array`).

## Repro
```
CV=target/release/cratonvm.exe; JDK="C:/Program Files/Java/jdk-25"
STD=.bench-cache/junit-platform-console-standalone-1.10.2.jar
$CV --java-home "$JDK" -cp "<abs cp>" org.junit.platform.console.ConsoleLauncher \
    execute --select-class org.apache.commons.math4.transform.TransformUtilsTest --disable-banner 2>&1 \
  | grep -a "ARRAY-LEN-GUARD"
```
(abs cp recipe in `continue_prompt_junit5_discovery.md`.) Markers: `grep PB-START-ENTRY`
/ `PB-START-OLD` to confirm neither start native runs.
