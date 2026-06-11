# CratonVM — Open JVM Failures

Dev HEAD: `85c80dfd`. Last updated: 2026-06-10.

---

## Recently resolved (2026-06-10)

### JIT dispatch exception wrapping — ✅ FIXED
**File:** `docs/gaps/gap-jit-dispatch-exception-wrapping.md`  
Already fixed in `vm/src/jit/helpers.rs::handle_jit_dispatch_error` (match arms for
`VmError::Runtime`, `VmError::Linkage` (all `LinkageError` variants → matching `java.lang.*`),
and `VmError::ClassFile(ClassNotFound)` → `NoClassDefFoundError`, before the catch-all
`InternalError` wrap). The entry had not been moved out of "open" when the fix landed.

### `@Retention(CLASS)` annotations visible at runtime — ✅ FIXED
**File:** `docs/gaps/gap-annotation-retention-policy.md`  
Reflection now only registers `RuntimeVisibleAnnotations` (`@Retention(RUNTIME)`).
`RuntimeInvisibleAnnotations` (`@Retention(CLASS)`) are filtered at every reflection
source: `class.annotations` (3 spots in `classloading/src/class_manager.rs`: define,
redefine, in-place-update) and method/field annotations
(`extract_annotations_from_attributes` in `vm/src/vm/vm_exec.rs`).

---

## High

### WildFly testSubsystem — 300s TIMEOUT → ✅ FIXED 2026-06-11
**File:** `apps/wildfly/CRATONVM_BUGS.md` (Bug 4)  
`wildfly-health` now passes: `RESULT tests=2 failures=0 ok=true` in **12.5s** (was TIMEOUT 300s).

**Root cause:** CratonVM registered a **partial synthetic Rust-backed shadow** of
`org.jboss.threads.EnhancedQueueExecutor` (jboss-msc's service-container executor) —
`Builder.build()`, `execute()`, `shutdown*()`, `isShutdown/isTerminated/awaitTermination`
(`native-builtins/src/wildfly_core.rs`). The synthetic `build()` allocates a 4-field stub
and **never runs the real `<init>(Builder)`**, so the real `threadStatus` long (which packs
core/max pool size, accessed via a `long[]` array-element VarHandle) stays 0.
`getCorePoolSize()`/`getMaximumPoolSize()` are **not** shimmed → real bytecode reads
`threadStatus`=0 → both return 0 → the executor believes its max pool size is 0 → `execute()`
never spawns a worker → **every task submitted via jboss-msc is dropped**, so
`ModelControllerService` never starts → no "Controller Boot Thread" → the subsystem-test
`waitForSetup()` `CountDownLatch` is never counted down → `testSubsystem` hangs 300s.

**Fix:** gate the synthetic EQE natives behind `CRATONVM_SYNTHETIC_EQE` (default OFF) so the
**real jboss-threads bytecode** runs (it initializes `threadStatus` and spawns real worker
`Thread`s, all verified working in isolation). Same partial-shadow fix pattern as Phaser /
BlockingQueue. Minimal repro (no WildFly):
`new EnhancedQueueExecutor.Builder().setCorePoolSize(1).setMaximumPoolSize(4).build().getCorePoolSize()`
returned 0 (HotSpot 1); now returns 1 and submitted tasks run.

**Diagnostic journey (for reference):** the `$Factory` `out-of-bounds field read dropped`
warnings were a red herring — benign bounded noise from a one-time 8-element `TreeSet` build
where `natural_compare` probes slot 0 of stateless 0-field factory singletons and correctly
falls through to `compareTo` (now suppressed via an `object_num_fields >= 1` guard in
`native-collections/src/lib.rs`). The real hang was localized via `--stack-dump-on-timeout`
(augmented this session to also unpark `LockSupport.park` waiters and dump a per-thread
summary): only `main` + one JDK `InnocuousThread` existed (no boot thread), and a
`CRATONVM_DBG_FIELDADDR` field trace showed the EQE's `unsharedLongs`/`threadStatus` were
never written — pinpointing the synthetic `build()` that skips the real `<init>`.

### BC math-ec timeout (F2m JIT ban)
**File:** `docs/gaps/gap-bc-math-ec-crypto-regression-timeout.md`  
**Symptom:** `bc-math-ec` TIMEOUT — `F2m.LongArray.multiply()` JIT-banned due to miscompile; interpreted 60–225× slower than HotSpot.

### Phaser real-bytecode state — ✅ FIXED 2026-06-11
**File:** `docs/gaps/gap-phaser-real-bytecode-state.md`  
**Was:** `java.util.concurrent.Phaser` misexecuted in real-JDK builds (`state` reads 0,
`root` reads null → NPE in `reconcileState`).  
**Root cause:** a *partial* synthetic Phaser native shadow (`native-collections`
`register_phaser_natives`, called from the ungated `register_collections_natives`)
wrote a synthetic 3-int layout over the real 5-field layout, so the un-shadowed
`getUnarrivedParties` ran real bytecode against a null `root`.  
**Fix:** gate the call behind `#[cfg(feature = "synthetic-jdk")]` (mirroring the
adjacent already-gated `register_blocking_queue_natives`) so all Phaser methods run
real bytecode. Verified CratonVM == HotSpot incl. a 2-thread `arriveAndAwaitAdvance`
barrier.
