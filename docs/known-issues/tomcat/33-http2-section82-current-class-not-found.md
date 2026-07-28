# 33 — `TestHttp2Section_8_2` dies with `internal error: current class not found`  (OPEN)

**Status:** OPEN. Surfaced 2026-07-27 while retiring known-issue 04. This class
used to be classified HANG @1500 s and was carried as throughput evidence; it
is not a throughput item.

## Symptom

`org.apache.coyote.http2.TestHttp2Section_8_2` now runs its **252**
parameterized cases to completion and then fails:

```
java.lang.InternalError: JIT dispatch into
    org/junit/runner/Runner.run(Lorg/junit/runner/notification/RunNotifier;)V
    failed: internal error: current class not found
        at org.junit.runners.ParentRunner.runChildren(ParentRunner.java:329)
        …
Tests run: 252,  Failures: 1
```

| binary | wall | outcome |
|---|---|---|
| pre-fix (`origin/dev` @ `750213dab`) | 387 s | VM aborts: `main-vm run() returned Err: … internal error: current class not found` |
| with `e3cb2ab17` (JIT direct-call fix) | 332.9 s | same error, surfaced as a JUnit failure instead of a VM abort |
| HotSpot | 219.6 s | PASS |

So it is **pre-existing** — the direct-call fix only made it ~15 % faster and
changed where the error is caught. At 1.5× HotSpot this class is no longer
slow; it is broken.

## What is known

* `"current class not found"` is the interpreter's error for
  `class_manager.get_class(frame.class_id)` returning `None` — a frame whose
  `ClassId` is no longer in the class manager. There are a dozen sites that
  produce that exact string (`vm/src/runtime/interpreter.rs`); the JUnit stack
  shows the failing frame is an *interface* dispatch (`Runner.run`) reached
  from `ParentRunner.runChildren`, i.e. plain JUnit machinery, not HTTP/2 code.
* The class runs **252 cases, each starting and stopping a connector**, so by
  the time it fails the VM has created and discarded a large number of
  contexts/loaders. A `ClassId` that was valid and then is not points at class
  **unloading** — cf. the `DefaultInstanceManager` class-unload off-by-one
  family (`docs/internal/fixed-suite-bugs/tomcat/
  defaultinstancemanager-classunload-offbyone-recurrence-FIXED.md`) and
  `reference: parked thread's JIT memo caches pin graphs`.

## Next step

Re-run under `CRATONVM_DBG_JITC=1` plus whatever class-unload tracing exists
and capture the `ClassId` that goes missing, and the collection that removed
it. The failure is deterministic (2/2 runs, both binaries), so a bisect on the
case index is practical: `RunMethods` can restrict the run to a subset of the
252 cases in a single fixture.

## Reproduction

```
apps\tomcat-suite-runner\run-one.ps1 -Exe <exe> `
  -Class org.apache.coyote.http2.TestHttp2Section_8_2 -TimeoutSec 1500 -LogFile <log>
```
