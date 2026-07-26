# `DevToolPropertiesIntegrationTests` / `DevToolsEmbeddedDataSourceAutoConfigurationTests`: FIXED

**Status: FIXED — 2026-07-25.** Both classes now pass completely
(`DevToolPropertiesIntegrationTests` 5/5, `DevToolsEmbeddedDataSourceAutoConfigurationTests`
4/4) on a clean rebuild, confirmed against the fixture at
`/data/data/spring-boot-tomcat-crossmodule-20260717` (Azure host,
`victor@20.83.144.174`).

## Root cause

**Not** a classloader race, a lock-ordering deadlock across threads, or a
silent-jar-read race as this doc's original hypotheses guessed — it was a
single-thread self-deadlock caused by a Rust temporary-lifetime bug, unrelated
to any of them.

`array_is_assignable_to_impl`'s `resolve_component` closure in
`vm/src/runtime/interpreter.rs` (checkcast/instanceof array-component
resolution) chained:

```rust
shared.classes.class_manager.read()
    .find_class_by_name(name)
    .or_else(|| shared.load_class_concurrent(name).ok())
```

Rust extends a temporary's lifetime to the end of the *enclosing statement*.
Because the whole `.read()....or_else(...)` chain is one statement, the
`RwLockReadGuard` produced by `.read()` was **not dropped** until after the
`or_else` closure itself finished running — including the case where the
closure's `load_class_concurrent` call needed `class_manager.write()`. A
write-lock request queued behind that still-live read guard on the *same*
thread self-deadlocks forever against `parking_lot::RwLock` (which is not
reentrant).

This only manifests when an array-component class genuinely isn't loaded yet
at the moment of a `checkcast`/`instanceof`/`aastore` check (the
`find_class_by_name` miss that falls through to `load_class_concurrent`) —
rare for ordinary JDK types (already loaded at boot) but common for
ByteBuddy/Mockito's freshly-generated mock subclasses and their array types,
which is exactly the shape of both affected classes and explains why this
looked host-load/test-order dependent rather than deterministic: the race
window is "does this specific array-component name already have an entry",
not a timing race in the traditional sense.

Confirmed via a live `gdb` backtrace on the actual hung process
(`sudo -n gdb -p <pid> -batch -ex 'thread apply all bt'`; `ptrace_scope=1`
on this host requires `sudo -n`, which is passwordless here) — the main
thread was blocked in `parking_lot::raw_rwlock::wait_for_readers` inside
`class_manager.write()`, called from `load_class_concurrent`, called from
`array_is_assignable_to_impl`'s `resolve_component` closure, called from
`execute_instruction`'s array-`instanceof` handling — a direct, unambiguous
confirmation of the mechanism above.

## Fix

Bind the read result to a `let` first so the guard drops before any write-lock
attempt:

```rust
let found = shared.classes.class_manager.read().find_class_by_name(name);
found.or_else(|| shared.load_class_concurrent(name).ok())
```

`vm/src/runtime/interpreter.rs`, `array_is_assignable_to_impl`'s
`resolve_component` closure.

## A related, separate fix bundled in the same commit

While reproducing this class family, `TomcatServletWebServerServletContextListenerTests`
(`module/spring-boot-tomcat`, same `@ForkedClassPath` + Mockito shape) hit a
**different** bug after this deadlock fix cleared the hang: `verify(mock)`
throwing `NotAMockException` for a mock that was just created and used
successfully. Root-caused (not the deadlock, not GC, not identity-hash
instability) to an interpreter dispatch defect: `Object.equals()` calls
inside `ConcurrentHashMap`'s own internals were not reliably reaching
Mockito's `WeakConcurrentMap$WeakKey`/`$LatentKey` classes' real bytecode
`equals()` overrides — the exact same defect class an existing native bridge
(`native_brave_weak_key_equals`) already worked around for `brave`'s
identically-shaped `WeakKey`. Added matching bridges
(`native_mockito_weak_key_equals`/`native_mockito_latent_key_equals`,
`native-builtins/src/reference.rs`) for Mockito's own `WeakKey`/`LatentKey`.
Confirmed correct in isolation (identical referents compare equal, distinct
referents compare unequal) — a genuine fix for a real defect, matching the
Brave precedent — but it does **not**, by itself, close
`TomcatServletWebServerServletContextListenerTests`'s residual: a second,
independent root cause exists in classloader parent-delegation. See
[`tomcatservletwebserverservletcontextlistenertests-mockito-forkedclasspath-mockmethodadvice.md`](../../known-issues/springboot/tomcatservletwebserverservletcontextlistenertests-mockito-forkedclasspath-mockmethodadvice.md)
for the full, precise write-up of that remaining, still-open issue — it is
**not** the same bug as this doc's original hang/`NoClassDefFoundError`,
despite the 2026-07-24 update below having assumed they were the same.

## Regression check

70-class sweep across `module/spring-boot-devtools` (51 classes) and
`module/spring-boot-tomcat` (19 classes) on the fixed binary: 61/70 clean.
The remaining 9 are explainable and unrelated to this fix: a handful of
isolated single-test failures from a shared-`/tmp`-across-parallel-runs
sweep-harness artifact (`Existing directory ... does not have the
permissions [OWNER_READ, OWNER_WRITE, OWNER_EXECUTE]` — a collision from
reusing one `java.io.tmpdir` across 6 concurrent JVMs across repeated sweep
runs, not a VM bug), two classes timing out at the sweep's 90s cap under
host load (large classes, `TomcatServletWebServerFactoryTests` has 132 tests;
`LiveReloadServerTests` is socket-bound), and the one expected,
already-documented `TomcatServletWebServerServletContextListenerTests`
failure described above.

---

