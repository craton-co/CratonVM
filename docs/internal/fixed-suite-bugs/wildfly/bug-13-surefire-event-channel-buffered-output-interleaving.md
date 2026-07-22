# Bug 13: Surefire event channel frames were interleaved by native output paths

Status: FIXED
Date found: 2026-07-05
Area: WildFly suite runner, Surefire fork protocol, java.io output natives

## Symptom

WildFly domain tests under Surefire produced repeated dumpstream warnings and either crashed the fork or reported zero tests:

```text
Corrupted channel by directly writing to native stream in forked JVM 1
The forked VM terminated without properly saying goodbye
Tests run: 0
```

The dumpstream contained spliced Surefire frame fragments such as `maven-surefire-event`, `std-out-stream`, `sys-prop`, stack-trace lines, and WildFly host-controller output in the same byte ranges.

## Root Cause

Two CratonVM gaps combined:

1. Native `PrintStream` fallbacks could see Surefire's `ConsoleOutputCapture$ForwardingPrintStream` and route output through generic fd/stream handling instead of preserving Surefire's `TestOutputReceiver` framing.
2. `BufferedOutputStream.write([BII)` copied bytes one at a time into the shared buffer without a native synchronization boundary. Surefire encodes one event as one `byte[]` write; concurrent stdout/sysprop/test events could interleave at byte granularity before flush.

## Fix

- `native-builtins/src/lib.rs` detects Surefire's forwarding print stream and emits `TestOutputReportEntry` objects to `target.writeTestOutput(...)` before any fd fast path.
- `native-io/src/lib.rs` wraps `BufferedOutputStream` write/writeBulk/flush with a reentrant native lock so a bulk event frame is copied atomically.

## Verification

Focused no-JIT rerun with `cratonvm-wildfly-nonpassed-20260705-035722-surefireps2`:

```text
/data/wt/wt-wildfly-nonpassed-20260705-035722/apps/wildfly-suite-runner/out/azure-defaultconfig-surefireps2-nojit-078-nojit-real-failed-20260705-152923
```

Result changed from Surefire crash/empty run to real JUnit results:

```text
classes: FAIL=1
test-methods: found=2 passed=0 failed=0 errors=2
```

No new `.dumpstream` file was created for the `surefireps2` run; the latest dumpstream remains from the prior `surefireps1` run at `2026-07-05T15-24-12_686-jvmRun1.dumpstream`.
