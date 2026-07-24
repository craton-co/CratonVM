# Keycloak test-framework: Phaser/ForkJoinPool hang in SmallRye Sisu bean loading — FIXED

Status: FIXED (branch `fix/phaser-forkjoinpool-managedblock-hang-20260707`, commit `743da7b1`, merged to `dev`)

Date observed: 2026-07-06. Root-caused and fixed: 2026-07-07.

## Original symptom

With [the `LinkedList.addAll` dependency-resolution bug fixed](keycloak-testframework-linkedlist-addall-deployrequestedinstances-FIXED.md),
`tests/base :: org.keycloak.tests.admin.identityprovider.IdentityProviderMapperTest`
progressed into `DistributionKeycloakServer.start()` → `ProviderDeployer.updateDependencies()`
→ Quarkus's Maven bootstrap, then **hung indefinitely** (confirmed 12+ hours, idle/blocked
not spinning). A `--stack-dump-on-timeout 90` run captured the sole visible thread blocked in:

```
java/util/concurrent/Phaser$QNode.block()
java/util/concurrent/ForkJoinPool.unmanagedBlock/managedBlock
java/util/concurrent/Phaser.internalAwaitAdvance/arriveAndAwaitAdvance
io/smallrye/beanbag/sisu/BeanLoadingTaskRunner.waitForCompletion()
io/smallrye/beanbag/sisu/Sisu.addClassLoader(...)
```

Real HotSpot completes the same path in ~30s.

## Root cause

The initial hypothesis (a `ForkJoinPool.execute(ForkJoinTask)`/silently-swallowed
`exec()` issue) was **wrong** — decompiling the actual `smallrye-beanbag-sisu-1.6.1.jar`
showed `BeanLoadingTaskRunner` never touches `ForkJoinPool` at all. It dispatches work
via `CompletableFuture.runAsync(Runnable)` and coordinates completion with a `Phaser`
(`register()` per task, `arriveAndDeregister()` inside a proper `try`/`catch(Exception)`/
`finally`, `arriveAndAwaitAdvance()` on the waiting thread).

The real bug: `../../../../native-builtins/src/phases_late.rs` (`CompletableFuture.runAsync(Runnable)`
and the `(Runnable, Executor)` overload, ~lines 689-746) ran the submitted `Runnable`
eagerly via `ctx.invoke_virtual(runnable, "run", "()V", &[])` and **unconditionally
swallowed any `Err`** into a stringified error field on a synthetic `CompletableFuture`,
always reporting success back to the caller.

This is harmless for a genuine Java exception (`MethodCallFailed::ExceptionThrown`)
because CratonVM's interpreter exception-table dispatch already runs the callee's own
`catch`/`finally` handlers before `invoke_virtual` returns. But
`MethodCallFailed::InternalError` (`types/src/error.rs:37-44`, explicitly documented as
"not catchable by Java catch blocks... abort execution entirely") tears down the Java
call stack **without ever routing through the callee's bytecode exception table** — so
if any VM-level gap fires deep inside Sisu's bean-loading work, the task's
`finally { phaser.arriveAndDeregister(); }` never runs, permanently desyncing the
Phaser's party count and hanging `arriveAndAwaitAdvance()` forever.

## Fix

`../../../../native-builtins/src/phases_late.rs` (both `runAsync` overloads, ~lines 695-699 and
726-733): check `matches!(result, Err(MethodCallFailed::InternalError(_)))` immediately
after the inline `invoke_virtual` call and propagate it out of the native method instead
of absorbing it into the synthetic `CompletableFuture`.

Verified:
- `cargo test --release -p cratonvm-native-builtins --features synthetic-jdk,experimental-serialization phases_late`: 75 passed, 0 failed, 4 pre-existing ignores — no regressions.
- Minimal repro (`CfPhaserRepro.java`: `Phaser` + 4x `CompletableFuture.runAsync` with try/catch/finally, matching the confirmed Sisu bytecode shape) passes in both "clean" and "one task throws a caught exception" modes.
- Real Keycloak class `org.keycloak.tests.admin.identityprovider.IdentityProviderMapperTest`: before fix hung 12+ hours; after fix completes in ~71s. All 5 tests now fail fast on a distinct, legitimate, pre-existing, already-triaged-as-not-a-CratonVM-bug issue — the [`keycloak-test-framework-remote-providers` Maven artifact resolution gap](keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md) (confirmed 2026-07-07 to reproduce identically on real HotSpot) — not a repeat of the Phaser hang signature.

## Evidence

- Fix branch: `fix/phaser-forkjoinpool-managedblock-hang-20260707`, worktree `C:\craton\CratonVM-phaser-fjp-hang-20260707`, commit `743da7b1`. Merged to `dev`.
