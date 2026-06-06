# JIT native-stack recursion guard + short-hot-method promotion

Two related JIT bugs, both pass on HotSpot. Fix touches ONLY
`vm/src/jit/helpers.rs` and `vm/src/runtime/interpreter.rs`.

## BUG 1 — bintrees18 native Rust stack overflow (rc=127)

`bench/BenchSuite.java binaryTrees(18)` (deep self-recursive `make(int)`)
crashed CratonVM with a native Rust stack overflow once the recursive methods
were JIT-compiled. The JIT→JIT dispatch path
(`jit_invoke_dispatch` / `jit_invoke_virtual_mic` → compiled entry → …) never
re-enters `interpreter::execute`, so the interpreter's `EXEC_DEPTH` /
`EXEC_DEPTH_CEILING` guard (which throws a *catchable* `StackOverflowError`)
never fired. Each compiled recursion level stacked a large native frame with
nothing checking remaining OS stack → guard-page overflow → uncatchable abort.

### Fix
- `interpreter.rs`: new per-thread JIT-dispatch ceiling
  `JIT_DISPATCH_DEPTH_CEILING`, derived from the thread's REAL native stack size
  by `derive_jit_dispatch_depth_ceiling` (32 KiB/level budget — the JIT dispatch
  frame is ~4× an interpreter level — half the stack, floor
  `MIN_JIT_DISPATCH_DEPTH_CEILING = 64`). Set alongside the exec ceiling in
  `init_thread_exec_depth_ceiling`; read via `pub fn jit_dispatch_depth_ceiling`.
  Main thread (128 MiB) → 2048 levels; 8 MiB worker → floored 64.
- `helpers.rs`: thread-local `JIT_DISPATCH_DEPTH` counter + RAII
  `JitDispatchDepthGuard` (Drop-decrement). `enter_jit_dispatch(vm)` bumps the
  depth, checks the ceiling, and on overflow rolls back, stashes a catchable
  `java/lang/StackOverflowError` via `raise_jit_stack_overflow`
  (`create_exception_object` + `set_jit_pending_exception`, mirroring
  `jit_newarray_oom`), and returns the `i64::MIN` deopt sentinel. Wired into the
  top of both `jit_invoke_dispatch` and `jit_invoke_virtual_mic` (after the arg
  slice is formed, before any compiled-callee recursion). The interpreter's
  post-JIT drain (`take_jit_pending_exception`) routes the SOE through the
  method's exception table — catchable, not a process abort.

Result: bintrees18 completes (checksum 68332206); genuinely-infinite compiled
recursion throws a catchable `StackOverflowError`.

## BUG 2 — pqc-crypto-regression timeout (hot methods never JIT-compiled)

BC `org.bouncycastle.pqc.crypto.test.RegressionTest` — HotSpot ~2 s, CratonVM
TIMEOUT (>360 s). Slow, not stuck: short-but-astronomically-hot methods reached
through all-interpreted call trees (`Permute.permute`, `ChaChaEngine.chachaCore`,
`HashFunctions.hash_n_n`, `Salsa20Engine.processBytes`) got neither a normal nor
an OSR compile:
- the invocation gate `invoc_count >= T && invoc_count % T == 0` fired only at
  exact multiples of `T`, so a single transient upgrade-gate failure at `T`
  pushed the next attempt out another full `T` calls;
- OSR's `backward_count` resets every invocation, so methods that loop only a
  few dozen times per call never reach `OSR_THRESHOLD` — the invocation counter
  is their only road to the JIT.

### Fix (`interpreter.rs`, cached-bytecode dispatch ~12169)
- Lower `JIT_INVOCATION_THRESHOLD` 2000 → 500 so short hot methods promote
  sooner.
- Replace the exact-multiple gate with: fire on the first crossing
  (`invoc_count == T`), then re-attempt every `JIT_RETRY_STRIDE = 64` calls
  until the upgrade succeeds. A transient failure now recovers within ~64 calls
  instead of another full threshold. `increment_invocation` is a persistent
  per-method counter, so the bar is crossed even from the interpreted-call-tree
  case; a successful upgrade rewrites the invoke cache to `Jit`, so this counting
  block is bypassed afterward and there is no ongoing re-spam. Normal cold-method
  warmup is preserved (still waits `T` calls before the first attempt).
