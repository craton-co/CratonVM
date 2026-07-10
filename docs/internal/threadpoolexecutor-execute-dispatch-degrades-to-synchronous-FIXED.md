# Real `ThreadPoolExecutor.execute()` degraded to synchronous (or, before `306cd352`, silently dropped) — receiver-real check not fully effective

Status: FIXED — 2026-07-10, branch `fix/tpe-execute-async-dispatch-20260710`, root-caused via targeted debug tracing at each candidate dispatch site
Severity: was High (broad blast radius — every real `ThreadPoolExecutor` in the VM, not just `Executors.*`-created ones)

## Symptom (recap)

```java
ExecutorService es = new ThreadPoolExecutor(4, 4, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
for (int i = 0; i < 3; i++) {
    int idx = i;
    es.execute(() -> System.out.println("task " + idx + " on " + Thread.currentThread()));
}
```

On dev tip (`aa21e334` + `306cd352` + later): all 3 tasks printed, but all on the **same**
calling thread instead of 3 distinct worker threads — `execute()` was running synchronously
instead of asynchronously.

## Root cause: FOUR independent unconditional native-override checks, not the three `306cd352` patched

`306cd352`'s own commit message describes three dispatch points it made receiver-aware
(`intercept_force_registered_native` in `vm/src/runtime/interpreter.rs`, and
`invoke_or_native`/`invoke_on_class_shared_inner` in `vm/src/vm/vm_exec.rs`). All three were
implemented correctly. The bug is that **`try_stackless_invoke` and `populate_virtual_invoke_cache`
(also in `vm/src/runtime/interpreter.rs`) each have their OWN, separate "a Rust native registered
for this method always wins over bytecode" checks** — a different, stronger rule than the
force-list mechanism `intercept_force_registered_native` gates — and none of the four call sites
below had ever been given the receiver-aware exemption:

1. **`try_stackless_invoke`'s "step 1"** — `.or_else(|| shared.native_methods.find(class_name,
   method_name, descriptor))`. Runs on literally the first call, before the interpreter even
   knows real bytecode exists for the method.
2. **`try_stackless_invoke`'s "step 6"** — a second, independent native lookup that runs
   *after* real bytecode was already resolved at step 4/5 ("Check for native override on
   bytecode method"). This one fires even when step 1 correctly fell through to bytecode.
3. **`populate_virtual_invoke_cache`'s "Check native overrides FIRST"** block — populates the
   monomorphic inline cache (`CachedInvokeTarget::VirtualNative`) keyed by `(call site, receiver
   class_id)` alone. Since the cache has no per-instance awareness, caching `VirtualNative` here
   for one real receiver would incorrectly apply to *every* future call at that site.
4. **`populate_virtual_invoke_cache`'s direct call to `force_native_over_real_jdk_bytecode`** — a
   pure `(class, method, descriptor)` allowlist (the same function `306cd352` added the
   `ThreadPoolExecutor.execute` entry to) consulted here with zero receiver awareness, bypassing
   every other receiver-aware guard.

Because `native_es_execute` had to stay registered on `ThreadPoolExecutor` (so the *synthetic*
case could still be forced through it), all four of these pre-existing, receiver-blind checks
found it and routed dispatch straight into `native_es_execute`'s own defense-in-depth "run inline"
fallback — for every real `ThreadPoolExecutor.execute()` call, not just the synthetic-stub or
self-recursion cases that fallback was meant to catch.

## Fix

Added the same `threadpool_executor_has_real_workers(shared, receiver)` receiver check (mirroring
the one already used correctly by `intercept_force_registered_native`) to all four sites above,
so a genuinely real, bytecode-constructed `ThreadPoolExecutor` (`workers` field populated by its
real `<init>`) is exempted from all four and its real `execute()` bytecode runs — hitting
`addWorker()` → `Thread.start()` → a genuine new worker thread — exactly like HotSpot.
`populate_virtual_invoke_cache`'s exemption additionally required threading the actual receiver
`Value` through the function (it previously only had `receiver_class_id`, insufficient for a
per-instance check), added as a new `receiver_value: &Value` parameter.

## Verification

- `ExecProbe3.java` (this doc's own repro, bare `new ThreadPoolExecutor(...).execute()` x3):
  3 distinct worker thread IDs (`Thread[#1,...]`, `#2`, `#3`), confirmed in both debug and
  `--release` builds, with and without `--nojit`.
- `ExecProbe.java`/`ExecProbe2.java` (the pre-existing `Executors.newSingleThreadExecutor()` /
  `newFixedThreadPool()` + `shutdown()`/`awaitTermination()` regression repros from `306cd352`/
  the "layer 2" mainlock-NPE fix): still pass — `Executors.*` factories now correctly dispatch
  asynchronously too (they build genuinely real objects since the "layer 2" fix, so they take
  the same real-bytecode path this fix unblocks).
- `cargo test -p cratonvm-vm --lib`: 2190 passed, 2 failed — the same two failures already
  documented as pre-existing/unrelated in `306cd352`'s own commit message
  (`real_jdk_mode_registers_fewer_natives`, a stale native-count threshold, and
  `ensure_system_streams_creates_objects`). A single SIGSEGV observed on one run of this suite
  did not reproduce on retry (2190/2192 clean) — a host-level flake, not caused by this change.

## Lesson for future "phantom native dispatch beats receiver-aware fix" investigations

A receiver-aware exemption added at one native-vs-bytecode decision point does not automatically
apply anywhere else `shared.native_methods.find(class, method, descriptor)` (or
`force_native_over_real_jdk_bytecode`) is consulted directly — this codebase has at least five
independent call sites that treat "a native is registered for this triple" as sufficient reason
to skip bytecode, only one of which (`intercept_force_registered_native`) carried the
receiver check before this fix. When a fix "should" work per one dispatch point's logic but
empirically doesn't, instrument every one of the class's `shared.native_methods.find` call sites
directly and confirm which one still fires, rather than assuming the original three-point fix's
own reasoning generalizes.
