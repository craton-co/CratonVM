# ES HANG - server org.elasticsearch.lucene.queries.LongRandomBinaryDocValuesRangeQueryTests

Status: FIXED

Observed in:
- Run: `es-nonpassed-rerun-20260708-191002`
- Mode/shard: `jit-shard3`
- VM/JIT: `craton` / `on`
- rc: `TIMEOUT`
- status: `HANG`
- seconds: `600.193`
- tests parsed: `0`
- failed parsed: `0`
- note: ``

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun`
- Branch used for collection: `codex/es-nonpassed-rerun-20260708-191002`
- Collection binary: `/data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002`
- Binary base dev SHA: `3d61003bbfdf9c6b045d29afefd45519dc558881`
- Docs generated after isolated worktree fast-forwarded to dev SHA: `8736a20b6e269bae3ec89d44e22117e2d4eba9a0`

Evidence files:
- stdout: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.LongRandomBinaryDocValuesRangeQueryTests.out.log`
- stderr: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002/jit-shard3/logs/server.org.elasticsearch.lucene.queries.LongRandomBinaryDocValuesRangeQueryTests.err.log`

Extracted stderr signals:
- `==== jstack at approximately timeout time ====`
- `NOTE: reproduce with: gradlew test --tests LongRandomBinaryDocValuesRangeQueryTests.testAllEqual -Dtests.seed=B17AC9D3E1F2A0C4 ...`
- `WARN [RandomizedRunner] Will linger awaiting termination of 3 leaked thread(s).`

## 2026-07-10 investigation and fix

The original 600s HANG (base SHA `3d61003b`) was re-investigated on the current
`dev` tip. On a binary built strictly after that SHA (but before this fix), the
symptom had CHANGED from a 600s suite-timeout hang to a **100% deterministic
SIGSEGV within ~2 seconds** under JIT — a genuine, unrelated new regression
sitting on top of (and masking) whatever the original hang mechanism was.

Bisected (`CRATONVM_JIT_BISECT_ONLY`, `CRATONVM_DBG_COMPACT_INLINE`,
`CRATONVM_DBG_JIT_DISASM`) to JIT-compiling `java.util.concurrent.locks.
ReentrantLock`/`ReentrantLock$Sync`: `ReentrantLock.unlock()`'s single getfield
(`this.sync`) was compiled as a 32-bit sign-extending `movsxd` load instead of
a 64-bit `mov`, truncating the loaded `Sync` reference into a garbage receiver
that then SIGSEGVed dispatching `sync.release(1)`.

Root cause: three `field_resolver` closures (`vm/src/runtime/interpreter.rs`)
called `compact_field_slot(...).unwrap_or((0, false))` — when a field's
declaring class has no registered compact layout, the miss was silently
turned into a fabricated "offset 0, not a reference" instead of "no compact
data available," and `jit/src/lib.rs`'s scan step trusted that fabrication
unconditionally, steering the getfield/putfield inline codegen
(`jit/src/x64.rs`) to treat a genuine reference field as a primitive.

This was independently root-caused and fixed by a concurrent session as
commit `7f96c26c` ("fix(jit): never fabricate (0,false) compact-field slots —
WildFly HC invoke-IC SIGSEGV") — same three closures, same mechanism, found
via a third, unrelated symptom (WildFly Host Controller). A second concurrent
session (`93b33576`) had separately worked around the SIGSEGV by flipping
`guarded_inline_getfield_enabled()` to opt-in; once `7f96c26c` landed, a third
commit (`be710234`) restored the default to ON.

**Verified 2026-07-10 on a clean checkout of dev tip `e768916a`** (no local
changes): `LongRandomBinaryDocValuesRangeQueryTests` passes cleanly under
default JIT settings — `OK (6 tests)`, ~20-30s, 0 failures, run 3x. The
original 600s hang does not reproduce; the JIT SIGSEGV that had started
masking it is gone.
