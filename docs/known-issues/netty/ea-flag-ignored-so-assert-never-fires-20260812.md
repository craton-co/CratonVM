# `-ea` is parsed and thrown away, so `assert` never fires

**Status:** OPEN (2026-08-12). Found while working
[investigate-batch-08.md](investigate-batch-08.md). Not a batch-08 blocker —
see "Why this was invisible" — but a real defect with a small, known fix.

## Symptom

Two batch-08 classes fail 6 tests each on CratonVM:

- `io.netty.handler.codec.http2.UniformStreamByteDistributorFlowControllerTest`
- `io.netty.handler.codec.http2.WeightedFairQueueRemoteFlowControllerTest`

```
org.opentest4j.AssertionFailedError: Expected java.lang.AssertionError to be thrown, but nothing was thrown.
    at io.netty.handler.codec.http2.DefaultHttp2RemoteFlowControllerTest.invalidWeightTooBigThrows(...:952)
```

The tests exercise netty's argument validation, which is written with Java
`assert` statements, so they only pass with assertions enabled.

## Why this was invisible

**The netty harness does not pass `-ea`.** Without it HotSpot fails these same
12 tests identically, so the suite run recorded "FAIL on both" and the classes
looked like an environment problem rather than a VM one. Adding `-ea` separates
them:

| | without `-ea` | with `-ea` |
|---|---|---|
| HotSpot JDK 25 | ok=28 failed=6 | **ok=34 failed=0** |
| CratonVM | ok=28 failed=6 | **ok=28 failed=6** |

HotSpot honours the flag and goes green. CratonVM ignores it and does not move.
The harness gap was hiding a genuine CratonVM defect behind a matching HotSpot
failure — the inverse of the usual "harness gap inflates the bug list".

## Root cause

CratonVM **does** implement assertion status. `native_assertion_status` backs
both `Class.desiredAssertionStatus()` and `desiredAssertionStatus0(Class)`, and
`assertion_status_default()` (`native-builtins/src/lib.rs`) returns 1 when the
`CRATONVM_ENABLE_ASSERTIONS` flag is set. With it on, `<clinit>` stores
`$assertionsDisabled = false` and real `assert` bytecode throws.

The command line never reaches that switch. `normalize_java_launcher_argv`
(`vm-cli/src/main.rs`) drops the whole assertion family on the floor:

```rust
// ... CratonVM does not implement assertion
// checking; silently ignore so Gradle/Maven forks that pass `-ea`
// unconditionally don't crash clap ...
else if a == "-ea" || a == "-da" || a == "-esa" || a == "-dsa"
    || a.starts_with("-ea:") || a.starts_with("-da:")
    || a.starts_with("-enableassertions") || ...
{
    i += 1;   // discarded
}
```

The comment is stale: assertion checking **is** implemented, just not reachable
from the flag every real launcher uses. Only the env var works today, which no
Maven Surefire or Gradle fork will ever set. (Surefire forks the test JVM with
`-ea` by default — so this affects far more than netty.)

## Suggested fix

Route the unscoped spellings to the existing switch:

- `-ea` / `-enableassertions` / `-esa` / `-enablesystemassertions` → enable
- `-da` / `-disableassertions` / `-dsa` / `-disablesystemassertions` → disable
- keep ignoring the **scoped** forms (`-ea:pkg...`, `-ea:Class`), because
  `assertion_status_default()` is a single global with no per-package
  granularity, and treating `-ea:some.pkg` as global-enable would switch on
  assertions for classes the caller deliberately excluded. Log them under
  `CRATONVM_DBG_ARGS` so the silence is discoverable.

The wrinkle worth care: `normalize_java_launcher_argv` is a **pure** function
with ~60 unit tests, and the flags it would have to influence latch on first
read (`FLAGS: OnceLock<VmFlags>`, `types/src/flags.rs`). Setting an env var
inside it would make those tests order-dependent and is the exact hazard
recorded in `declared-flags-latch-so-set-var-is-invisible-to-tests`. Do the
scan at the single production call site in `run()` instead — which owns process
setup and runs before any flag read — and leave the pure function pure.

Not fixed in the same change as the EC/TLS bug on this page: it is an
independent defect, it turns on behaviour (`assert` bodies) across the whole
corpus for anyone passing `-ea`, and it deserves its own suite validation
rather than riding along.

## Harness note

Separately from the VM fix, `apps/netty-suite-runner/common.args` passing `-ea`
would match what Maven Surefire does for netty's own CI, and would stop these
classes reading as environment noise. Doing so **before** the VM fix lands would
turn 12 currently-matching failures into 12 CratonVM-only failures, so sequence
the two.

The three `NativeImageHandlerMetadataTest` classes on the same page fail on both
VMs for an unrelated harness reason and are **not** VM defects:

```
Native Image reflection metadata is required ... not found under
  .../META-INF/native-image/null/null/generated/handlers/reflect-config.json
```

The `null/null` is the giveaway — the test builds that path from Maven
group/artifact system properties that Surefire sets and the fork-per-class
runner does not.
