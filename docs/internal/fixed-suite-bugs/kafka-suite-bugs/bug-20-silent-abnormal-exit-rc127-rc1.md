# Bug 20 — silent abnormal VM exit (rc=127 / rc=1) with no exception printed

**Severity:** High — a class's JVM exits abnormally mid-execution with **no
`RESULT` line and no Java exception/panic in the log**. Distinct from a clean test
failure. Reproduces under `--nojit`.

## Symptom
The per-class log ends shortly after startup (e.g. after the `BigInteger` post-clinit
fixup line) with nothing further; the process returns `rc=127` (or `rc=1`). No
`AbstractMethodError`, no assertion, no `[PANIC]`, no SIGSEGV backtrace — the VM
just terminates.

Observed (partial sweep):
- `consumer.ConsumerRecordTest` — rc=127, silent.
- `consumer.CooperativeStickyAssignorTest` — rc=1 (may carry an assignor error; see
  [bug-17](bug-17-assignor-assignment-mismatch.md)).

## Root cause (to pin down)
`rc=127`/`rc=1` with no diagnostic is the "silent ExitProcess-class" abnormal exit
documented in project memory (a Rust-side error that bypasses the Java exception
path and the panic/VEH handlers, or a `MethodCallFailed::InternalError` converted to
`bail!()`). Needs:
- run with `RUST_LOG=debug` / VEH backtrace enabled and `CRATONVM_SYMBOLIZE` to catch
  any native fault, and
- `CRATONVM_DBG_NSME=1` / clinit tracing to surface a swallowed `NoSuchMethodError` /
  `InternalError` escaping past the JIT boundary.
- Confirm it is **not** a cross-session `taskkill /F /IM cratonvm.exe` artifact
  (project memory: that also yields rc=1/empty) — here it is reproducible in
  isolation, so it is a real VM exit.

## Affected classes (partial — append more later)
- consumer.ConsumerRecordTest (rc=127)
- consumer.CooperativeStickyAssignorTest (rc=1)
- admin.DescribeUserScramCredentialsResultTest (rc=127) — overlaps
  [bug-08](bug-08-completablefuture-synthetic-layout-real-subclass.md) (KafkaFuture)
