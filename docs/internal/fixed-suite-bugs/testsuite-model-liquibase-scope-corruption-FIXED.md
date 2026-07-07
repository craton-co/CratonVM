# testsuite/model: Liquibase "Cannot end scope ... when currently at scope root" — FIXED

Status: FIXED (branch `fix/liquibase-scope-threadlocal-corruption-20260707`, commit
`752796a0`, merged to `dev`)

Date observed: 2026-07-06. Root-caused and fixed: 2026-07-07.

## Original symptom

15-16 of 18 sampled `testsuite/model` classes failed identically after ~200-280s with:

```
java.lang.ExceptionInInitializerError
    org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.validateSynch(...)
Caused by: liquibase.exception.LiquibaseException: java.lang.RuntimeException: Cannot end scope <random-id> when currently at scope root
```

## Root causes ruled out first

- **ThreadLocal-isolation / thread-pool-reuse** (the doc's original leading theory):
  ruled out — the call path (`KeycloakModelUtils.runJobInTransactionWithResult` →
  `suspendJtaTransaction` → `runnable.run()`) is entirely synchronous, single-thread,
  no `Executor`/`Thread` handoff anywhere in it.
- **try-with-resources double-`close()`**: ruled out by decompiling the real
  `liquibase-core-4.33.0.jar` — `liquibase.Scope` does not implement `AutoCloseable`
  and has no `close()` method at all; `Scope.enter()` returns a plain `String` id, so
  try-with-resources is structurally impossible here. Keycloak's own call sites
  (`DefaultLiquibaseConnectionProvider`/`QuarkusLiquibaseConnectionProvider`) call
  `Scope.enter()` exactly once at init and never call `exit()` (an intentional
  app-lifetime scope leak) — so the error must originate from *inside* Liquibase's own
  `Scope.child(...)` usage during changelog execution, which pairs `enter`/`exit` via a
  real exception-table try/finally in its bytecode and is exception-safe on its own.
- **`route_implicit_exc_through_callee` JIT recursive-exception-bail re-execution**
  (`vm/src/jit/helpers.rs:1176-1195`): initially suspected and reproduced a similar
  duplicate-side-effect signature with a nested-recursive `finally` repro, but
  independently ruled out as *this* bug's mechanism — `Scope.child`-shaped methods
  contain `athrow` and never reach JIT compilation at all under the existing RBC.6
  gate (confirmed via `CRATONVM_DBG_JIT_DISASM`).

## Actual root cause

`jit/src/x64.rs`'s `invokedynamic` (`0xba`) codegen unconditionally jumps to the JIT's
shared uncommon-trap deopt stub (`DeoptReason::UnreachedCode`) — the indy itself is
never JIT-executed by design. That trap's "safe reject" fallback (current behavior
since a same-day revert of a precise-resume fix that regressed Groovy) **re-runs the
whole method from its interpreter entry point** rather than resuming from the throw
point. Any committing side effect (`putstatic`/`putfield`/array-store/`invoke*`)
positioned *before* the invokedynamic in program order had already executed for real
during the JIT attempt, so the interpreter re-run executes it a second time.

Liquibase's `Scope.enter()`-shaped helpers do exactly this: a field mutation that
pushes a scope, immediately followed by a `makeConcatWithConstants` invokedynamic that
builds the scope's random id. Once JIT-compiled, every call double-pushed the scope
stack — a later `Scope.exit()` then saw a mismatched stack and threw "Cannot end scope
X when currently at scope root."

## Fix

`jit/src/x64.rs`, in `jit_scan`: tracks the earliest pc of any committing side effect;
if one precedes an `invokedynamic` site in raw pc order, `jit_scan` now returns `None`
(refuses to JIT-compile the whole method), matching the existing RBC.6 "when in doubt,
don't compile" posture. 91 lines added, single file.

Verified:
- Minimal repro (`enter()`-shaped method, `counter++` before a string-concat indy): `enterCount` doubled every call under JIT pre-fix (e.g. n=50000 → 99373); exact match (50000/50000) post-fix, both in simple and nested/recursive-lambda variants.
- The nested-recursive big repro from the initial (ruled-out) investigation (`FinallyReproNestedBig`, 800k iterations): pre-fix `exitCount=3175429` vs real-JDK `1987878` (~60% inflated, matching the reported symptom exactly); post-fix `exitCount=1987878`, deterministic across repeated runs — confirms this is the actual mechanism behind the originally-observed symptom.
- `cargo test -p cratonvm-jit --lib`: 878 passed, 4 failed — the same 4 pre-existing, documented-unrelated aarch64 branch-range failures; no new regressions.
- `cargo test -p cratonvm-vm --lib jit`: 158 passed, 0 failed.
- Real Keycloak `testsuite/model.clientscope.ClientScopeModelTest` via the suite runner: no longer hits the Scope error — now fails on a separate, pre-existing, non-regressed Infinispan protostream bytecode-decode issue (confirmed identical on the pre-fix binary too).
- Deliberate tradeoff: the JIT trap previously also covered genuinely-dead code (e.g. an unreachable `assert` branch after other side effects) — that case now also stays interpreted, ~40% slower on a narrow synthetic 2M-iteration micro-benchmark of that exact shape. Accepted as a documented, narrow correctness-over-performance tradeoff.

## Evidence

- Fix branch: `fix/liquibase-scope-threadlocal-corruption-20260707`, worktree `C:\craton\CratonVM-liquibase-scope-20260707`, commit `752796a0`. Merged to `dev`.
- Repro files: `FinallyReproNested.java` / `FinallyReproNestedBig.java` (scratch).
