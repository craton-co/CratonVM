# Real `ThreadPoolExecutor.execute()` degraded to synchronous (or, before `306cd352`, silently dropped) — receiver-real check not fully effective

Status: FIXED — 2026-07-10, by two independent, complementary fixes landed within the same
session-day (`fix/tpe-execute-async-dispatch-20260710` and `fix/wildfly-hib32-gate-20260710`,
reconciled here). Moved to `docs/internal/`.
Severity: was High (broad blast radius — every real `ThreadPoolExecutor` in the VM, not just
`Executors.*`-created ones)

## Symptom (recap)

```java
ExecutorService es = new ThreadPoolExecutor(4, 4, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
for (int i = 0; i < 3; i++) {
    int idx = i;
    es.execute(() -> System.out.println("task " + idx + " on " + Thread.currentThread()));
}
```

- On dev `4b08ffad`: prints 3 distinct real worker threads (`Thread[#1,Thread,5,main]`,
  `Thread[#2,...]`, `Thread[#3,...]`) — correct, matches HotSpot semantics (`execute()` returns
  immediately, tasks run async on pool workers).
- On dev `aa21e334` alone (no `306cd352`): **nothing prints at all** — the tasks are silently never
  run, no exception, no hang, `execute()` returns normally.
- On dev tip (`aa21e334` + `306cd352` + later): all 3 tasks print, but all on the **same**
  calling thread instead of 3 distinct worker threads — `execute()` was running synchronously
  instead of asynchronously.

## Root cause: (at least) five independent unconditional native-override checks, only one of which `306cd352` made receiver-aware

`306cd352`'s own commit message describes three dispatch points it made receiver-aware
(`intercept_force_registered_native` in `vm/src/runtime/interpreter.rs`, and
`invoke_or_native`/`invoke_on_class_shared_inner` in `vm/src/vm/vm_exec.rs`). All three were
implemented correctly, but at least two MORE independent, unconditional "a Rust native registered
for this method always wins" checks existed and were not patched:

1. **`try_stackless_invoke`'s "step 1"** — `.or_else(|| shared.native_methods.find(class_name,
   method_name, descriptor))`. Runs on literally the first call, before the interpreter even
   knows real bytecode exists for the method.
2. **`try_stackless_invoke`'s "step 6"** — a second, independent native lookup that runs
   *after* real bytecode was already resolved at step 4/5. This one fires even when step 1
   correctly fell through to bytecode.
3. **`populate_virtual_invoke_cache`'s "Check native overrides FIRST"** block — populates the
   monomorphic inline cache (`CachedInvokeTarget::VirtualNative`) keyed by `(call site, receiver
   class_id)` alone. Since the cache has no per-instance awareness, caching `VirtualNative` here
   for one real receiver would incorrectly apply to *every* future call at that site.
4. **`populate_virtual_invoke_cache`'s direct call to `force_native_over_real_jdk_bytecode`** — a
   pure `(class, method, descriptor)` allowlist (the same function `306cd352` added the
   `ThreadPoolExecutor.execute` entry to) consulted here with zero receiver awareness, bypassing
   every other receiver-aware guard.
5. **`invoke_on_class_shared_inner`'s `override_cb` block** (a different code path than the one
   `306cd352` patched in the same function) — an unconditional native re-check for any concrete
   (non-interface) declaring class, independent of `should_force_registered_native_over_bytecode`.

Because `native_es_execute` had to stay registered on `ThreadPoolExecutor` (so the *synthetic*
case could still be forced through it), all of these pre-existing, receiver-blind checks
found it and routed dispatch straight into `native_es_execute`'s own defense-in-depth "run inline"
fallback — for every real `ThreadPoolExecutor.execute()` call, not just the synthetic-stub or
self-recursion cases that fallback was meant to catch.

## Two independent, complementary fixes

Two sessions landed fixes for this the same day, taking different but compatible approaches —
both are on `dev`:

1. **`fix/tpe-execute-async-dispatch-20260710`** (point-patches sites 1-4 above): added the same
   `threadpool_executor_has_real_workers(shared, receiver)` receiver check (mirroring the one
   already used correctly by `intercept_force_registered_native`) to `try_stackless_invoke`'s two
   checks and both of `populate_virtual_invoke_cache`'s — the latter required threading the actual
   receiver `Value` through the function (previously only had `receiver_class_id`, insufficient
   for a per-instance check), added as a new `receiver_value: &Value` parameter.
2. **`fix/wildfly-hib32-gate-20260710`** (structural fix, covers site 5 and any future site):
   rather than continuing to enumerate and patch each unconditional-native-check call site
   one-by-one, removed the registry-level drop for `ThreadPoolExecutor` entirely and added
   `NativeContext::invoke_virtual_bytecode_only` (calls `interpreter::execute` directly, bypassing
   every native check unconditionally) — `native_es_execute`/`native_es_submit_*`/the `shutdown`
   closures check `executor_has_real_workers` themselves and redirect a genuinely-real receiver to
   real bytecode via this escape hatch, regardless of which dispatch path reached them. This also
   fixed `invoke_on_class_shared_inner`'s separate `override_cb` re-check (site 5, not covered by
   fix 1) and extended the same real-vs-synthetic awareness to `submit()`/`shutdown()`.

Both fixes are complementary, not conflicting: fix 1's point-patches make sites 1-4 exit to
bytecode *before* ever reaching `native_es_execute`; fix 2's callback-level check is what actually
handles any receiver that still reaches the native regardless (including site 5, and any future
unaudited dispatch point) — and is what the now-dead-code synchronous-inline fallback in
`native_es_execute` was replaced with.

## Verification

- `ExecProbe3.java` (this doc's own repro, bare `new ThreadPoolExecutor(...).execute()` x3):
  3 distinct worker thread IDs (`Thread[#1,...]`, `#2`, `#3`), confirmed in both debug and
  `--release` builds, with and without `--nojit`.
- `ExecProbe.java`/`ExecProbe2.java` (the pre-existing `Executors.newSingleThreadExecutor()` /
  `newFixedThreadPool()` + `shutdown()`/`awaitTermination()` regression repros from `306cd352`/
  the "layer 2" mainlock-NPE fix): still pass — `Executors.*` factories now correctly dispatch
  asynchronously too (they build genuinely real objects since the "layer 2" fix, so they take
  the same real-bytecode path this fix unblocks).
- `cargo test -p cratonvm-vm --lib`: 2193 passed, 2 failed — the same two failures already
  documented as pre-existing/unrelated in `306cd352`'s own commit message
  (`real_jdk_mode_registers_fewer_natives`, a stale native-count threshold, and
  `ensure_system_streams_creates_objects`).
- `cargo test -p cratonvm-native-api --lib` (179/179) and `-p cratonvm-native-builtins --lib`
  (2964/2965, the 1 pre-existing unrelated `ByteBuffer` failure) also pass.

## Lesson for future "phantom native dispatch beats receiver-aware fix" investigations

A receiver-aware exemption added at one native-vs-bytecode decision point does not automatically
apply anywhere else `shared.native_methods.find(class, method, descriptor)` (or
`force_native_over_real_jdk_bytecode`) is consulted directly — this codebase had at least five
independent call sites that treat "a native is registered for this triple" as sufficient reason
to skip bytecode, only one of which (`intercept_force_registered_native`) carried the
receiver check before these fixes. When a fix "should" work per one dispatch point's logic but
empirically doesn't, either (a) instrument every one of the class's `shared.native_methods.find`
call sites directly and confirm which one still fires, or (b) consider whether the callback
itself can be made receiver-aware and given an unconditional bytecode-only escape hatch
(`NativeContext::invoke_virtual_bytecode_only`), which sidesteps needing to enumerate every
dispatch site at all — see `docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`.

## Reproduction

Standalone (no WildFly/ES involved):

```java
import java.util.concurrent.*;
public class ExecProbe3 {
    public static void main(String[] args) throws Exception {
        ExecutorService es = new ThreadPoolExecutor(4, 4, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<>());
        for (int i = 0; i < 3; i++) {
            final int idx = i;
            es.execute(() -> System.out.println("  execute() task " + idx + " ran on " + Thread.currentThread()));
        }
        Thread.sleep(300);
        es.shutdown();
        es.awaitTermination(5, TimeUnit.SECONDS);
        System.out.println("DIRECT_CTOR_PROBE_OK");
    }
}
```
