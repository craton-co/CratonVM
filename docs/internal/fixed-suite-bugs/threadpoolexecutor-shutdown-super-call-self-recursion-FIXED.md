# FIXED: `ScheduledThreadPoolExecutor.shutdown()`'s `super.shutdown()` infinite-recursed into `StackOverflowError`

Status: FIXED
Fixed: 2026-07-10, branch `fix/stpe-shutdown-selfrecursion-20260710`, Azure host `victor@20.83.144.174`

## Symptom

During WildFly 32.0.1.Final domain boot (`CRATONVM_MSC_REAL_START=1`), once the Host
Controller invoke-inline-cache SIGSEGV (see
`wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver-FIXED.md`) was
fixed, boot progressed further and hit a new blocker: a real
`java.util.concurrent.ScheduledThreadPoolExecutor` (WildFly's
`HostControllerService$HostControllerScheduledExecutorService`) threw `StackOverflowError` the
first time anything called `.shutdown()` on it, with the identical frame
`java.util.concurrent.ScheduledThreadPoolExecutor.shutdown(ScheduledThreadPoolExecutor.java:842)`
repeated dozens of times.

Reproduces standalone, independent of WildFly, with:

```java
ScheduledExecutorService ses = Executors.newSingleThreadScheduledExecutor();
ses.schedule(() -> {}, 1, TimeUnit.MILLISECONDS);
Thread.sleep(50);
ses.shutdown();   // StackOverflowError
```

## Root cause

Real JDK `ScheduledThreadPoolExecutor` overrides `shutdown()`:

```java
public void shutdown() { super.shutdown(); }
```

`super.shutdown()` compiles to `invokespecial ThreadPoolExecutor.shutdown` — a **statically
bound** call to the immediate superclass's own method body, independent of the receiver's
dynamic type.

CratonVM's invokespecial cache population (`populate_invoke_cache`, `vm/src/runtime/
interpreter.rs`, the ONLY dispatch path that runs for `invokespecial`) checks the native
registry **first, unconditionally** — matching the documented
`..`/memory rule "invokespecial always prefers native over bytecode." A native IS
registered for `("java/util/concurrent/ThreadPoolExecutor", "shutdown", "()V")`
(`../../../native-builtins/src/lib.rs`, `register_executor_natives`), added earlier to make a
genuinely-real `ThreadPoolExecutor.execute()`/`.shutdown()` run its own real state machine
instead of writing CratonVM's synthetic 2-field placeholder's shutdown flag. That native's body:

```rust
if executor_has_real_workers(ctx, this) {
    ctx.invoke_virtual_bytecode_only(this, "shutdown", "()V", &[])?;
}
```

`invoke_virtual_bytecode_only` resolves the declaring class from **the receiver's dynamic
class** (`class_id_of(this)`) — correct for its OTHER call sites (`execute`/`submit`, reached via
ordinary invokevirtual on a receiver whose dynamic class IS the target), but wrong here: `this`'s
dynamic class is `ScheduledThreadPoolExecutor`, which overrides `shutdown()` — so
`invoke_virtual_bytecode_only` just re-finds **STPE's own overriding `shutdown()`** again, which
does `super.shutdown()` again, which hits the native again, forever.

## Fix

Added `NativeContext::invoke_special_bytecode_only(class_name, method_name, descriptor, args)` —
true invokespecial semantics (static binding on the **named** class, not the receiver's dynamic
class) that ALSO skips the native-registry-first check (unlike `invoke_special`, which would
re-find and re-invoke the very native calling it). Implementation
(`invoke_special_bytecode_only_shared`, `../../../vm/src/vm/vm_exec.rs`) is deliberately **independent**
of the existing `invoke_special_shared` — NOT a thin wrapper or shared tail with it — and
dispatches bytecode via `interpreter::execute` directly, **not**
`invoke_on_class_shared_no_retarget`. Two iterations were needed to land on this, both caught by
the standalone repro rather than the WildFly integration harness (which didn't reliably reach the
vulnerable call within its timeout in either failed attempt, making the direct repro the
load-bearing signal both times):

1. **First attempt** made the new function a thin wrapper sharing `invoke_special_shared`'s
   existing bytecode-only tail. This silently changed `invoke_special_shared`'s behavior for its
   OWN established callers (`Lookup.findSpecial`, interface default-method super calls), which
   rely on that tail's `invoke_on_class_shared_inner` machinery for lambda-proxy dispatch,
   `ACC_SYNCHRONIZED` monitor enter/exit, and a second declaring-class-keyed native lookup —
   too broad a blast radius for this bug fix, caught by re-running `cargo test -p cratonvm-vm
   --lib` before landing (12 pre-existing/unrelated failures were unaffected either way, but the
   shared-tail version was reverted regardless as the wrong design on inspection, not because it
   provably broke a test).
2. **Second attempt** kept the new function independent but still routed its bytecode fallback
   through `invoke_on_class_shared_no_retarget`. That function's `invoke_on_class_shared_inner`
   does its OWN unconditional native-registry re-check for any non-interface declaring class
   (`override_cb`) — routing through it re-created the exact same bug one level down, this time
   as **pure Rust-call recursion with no growing Java stack**: a raw native stack overflow /
   process abort instead of a clean `StackOverflowError`.

`NativeContext::invoke_virtual_bytecode_only`'s own doc comment already documents exactly this
`interpreter::execute`-direct pattern and the trap it avoids — the final fix follows that
established, already-production-proven precedent, with `invoke_special_bytecode_only_shared` as
a fully standalone function (own GC-pin, own class resolution, own direct `interpreter::execute`
call) that `invoke_special_shared` never calls into and vice versa.

Updated both `("java/util/concurrent/ExecutorService", "shutdown", "()V")` and
`("java/util/concurrent/ThreadPoolExecutor", "shutdown", "()V")` native registrations to call
`ctx.invoke_special_bytecode_only("java/util/concurrent/ThreadPoolExecutor", "shutdown", "()V",
&[Value::Object(Some(this))])` instead of `invoke_virtual_bytecode_only`.

## Verification

- Standalone repro (`StpeShutdownRepro.java`, above): pre-fix binary — `StackOverflowError`,
  43 repeated `ScheduledThreadPoolExecutor.shutdown` frames. First fix attempt (shared tail with
  `invoke_special_shared`) — not repro-tested in isolation (caught on inspection instead).
  Second fix attempt (independent function, but routed through
  `invoke_on_class_shared_no_retarget`) — **worse**: raw Rust stack overflow, process abort, no
  Java exception at all. Final fix (independent function, direct `interpreter::execute`) — clean
  pass: `shutdown() returned OK, isShutdown=false` (`isShutdown()` reading the synthetic flag on
  a real object is a separate, pre-existing, out-of-scope gap — unaffected by this fix).
- `cargo build --release -p cratonvm-native-api`: clean.
- `cargo test --release -p cratonvm-vm --lib`: 2180 passed, 12 failed — same 12 failures,
  byte-identical panic messages, both before AND after the final decoupling correction (a
  release-mode run of tests that assert debug-only lock-order enforcement is active, plus one
  stale native-registration-count threshold and one stale system-streams field assertion — all
  pre-existing/environmental, none touching invokespecial/executor dispatch).
- `cargo test --release -p cratonvm-native-builtins --lib -- executor`: 2/2 pass, no regression.
- WildFly domain boot (`CRATONVM_MSC_REAL_START=1`, same harness as the SIGSEGV fix): no
  `StackOverflowError`, no native stack-overflow abort, no respawn, boot proceeds the same
  distance as before (into `WFLYCTL0459`/management-services rollback — the next, separate,
  already-tracked front-line residual documented in
  `docs/known-issues/wildfly-domain-heap-corrupt-value-timeout.md`).

## Follow-ups (not fixed here, out of scope)

- `WFLYCTL0013`/`WFLYCTL0459` — `host=primary/core-service=management/management-interface=
  http-interface` `add` failing with `IllegalStateException: Container is down` — the next
  front-line blocker for the WildFly domain boot saga.
- `isShutdown()`/`isTerminated()` on a genuinely-real executor still reads the synthetic
  `EXEC_FIELD_SHUTDOWN` flag rather than the real object's actual state — pre-existing gap,
  gets away with it because nothing in this session's repro depended on the return value.
