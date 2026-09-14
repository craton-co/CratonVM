# Quarkus: `NoSuchMethodError: 'long cratonvm.synthetic.ProcessPipeInputStream.transferTo(java.io.OutputStream)'`

## Status
**OPEN, root-caused.** Discovered during Quarkus test suite failures triage (`run-20260914-052033-passed`).

## Symptom
Background process stream reading threads (e.g. `ForkedJvmEnvironment` or subprocess IO drain threads in Quarkus integration/deployment tests) fail asynchronously with a `NoSuchMethodError`:

```
Exception in thread "ForkedJvmEnvironment background thread" java.lang.NoSuchMethodError: 'long cratonvm.synthetic.ProcessPipeInputStream.transferTo(java.io.OutputStream)'
	at io.quarkus.deployment.cmd.ForkedJvmEnvironment$1.run(ForkedJvmEnvironment.java:45)
```

## Root Cause
CratonVM synthesizes custom subprocess stream objects (`cratonvm/synthetic/ProcessPipeInputStream`) in `native-io/src/process.rs` to handle subprocess stdio redirection and stream reading.

While `ProcessPipeInputStream` registers basic read methods:
- `read()` `()I`
- `read([BII)` `([BII)I`
- `read([B)` `([B)I`
- `available()` `()I`
- `close()` `()V`
- `readAllBytes()` `()[B`

it does **not** inherit or register an implementation for JDK 9+ `InputStream.transferTo(OutputStream out)`.

Because `cratonvm/synthetic/ProcessPipeInputStream`'s synthetic class chain does not resolve default/super methods from `java/io/InputStream` via normal VM class hierarchy dispatch, calling `transferTo` on a process input stream throws `NoSuchMethodError`.

## Affected Tests / Scenarios
- `io.quarkus.deployment.pkg.steps.ClassLoadingChainAnalyzerTest` (and all tests creating forked JVM environments or capturing process stdio using Java 9+ `InputStream.transferTo`).

## Remediation / Solution Plan
1. In `native-io/src/process.rs`, add a native implementation for `transferTo(Ljava/io/OutputStream;)J` on `SYNTHETIC_PROCESS_INPUT_STREAM` (or bridge virtual dispatch to standard loop read/write).
2. Per `docs/feature-designs/jdk-only-mode.md` and load-bearing rules in `AGENTS.md`, explicitly set the native registration category (`NativeKind::SyntheticStub` or `Bridge`), and update `native-builtins/tests/stub_ratchet.rs` ratchet baseline if necessary.
