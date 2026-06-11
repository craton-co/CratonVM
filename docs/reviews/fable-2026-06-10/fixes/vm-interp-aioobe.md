# vm-interp-aioobe — complete the Round-1 JIT pending-AIOOBE drain

## Finding
Round-1 made the JIT array load/store helpers in `vm/src/jit/helpers.rs` set a
thread-local `JIT_PENDING_AIOOBE` and (for loads) return the `i64::MIN` deopt
sentinel on out-of-bounds, exposing a public drain
`crate::jit::helpers::take_jit_pending_aioobe() -> Option<(i64,i64)>`. But the
interpreter only drained that flag *inside* the `result == i64::MIN` deopt arm
of the cached-JIT return path. A JIT **void-return store** helper
(`jit_iastore`/`jit_bastore`/`jit_aastore`/...) that hits an out-of-bounds index
sets the flag and returns **normally** — it cannot encode the `i64::MIN`
sentinel through a `void` return — so the pending AIOOBE was recorded and never
surfaced. It would then leak to the next unrelated JIT helper call (wrong PC /
wrong method) or be silently lost, masking a real `ArrayIndexOutOfBoundsException`
(JVMS violation), exactly mirroring the round-8/9 pending-**NPE** leak that was
already fixed on every JIT return path.

## Root cause
The pending-AIOOBE drain was never hoisted above the `i64::MIN` branch the way
the pending-NPE drain was. The three JIT-return choke points that drain NPE via
`take_jit_pending_npe()` had no equivalent normal-return AIOOBE drain:
1. the early-compile JIT path (stashes into `jit_early_exception`),
2. the OSR bail path (re-stashes; returns `None`),
3. the cached-JIT call path (routes via `route_jit_exception_through_method`).

## Exact change
Added an AIOOBE drain immediately **after** each NPE drain (NPE first, then
AIOOBE — a frame cannot have both pending at once, matching JVM semantics),
mirroring the *adjacent* NPE block's control-flow/return shape at each site. The
in-bounds fast path and the NPE logic are untouched. Because
`RuntimeError::ArrayIndexOutOfBoundsException` carries only `index` and
`vm/src/runtime/exceptions.rs` (not owned) maps it to message `None`, the
required message form `"Index <index> out of bounds for length <length>"` is
produced by building the throwable directly via
`crate::runtime::exceptions::create_exception_object(shared, thread,
"java/lang/ArrayIndexOutOfBoundsException", Some(&msg))` (the same helper the
`AbstractMethodError` path at ~line 2201 uses), then routing that `ObjectRef`.

- **Site 1 — early-compile path (~2962):** drained into a new `aioobe_routed`
  boolean, stashed the exception into `jit_early_exception`, and extended the
  fall-through guard `if !npe_routed && result != i64::MIN` to
  `if !npe_routed && !aioobe_routed && result != i64::MIN` so the post-frame-push
  handler walker (~line 2340) can catch it in the JIT'd method. `Err` arm
  propagates (rt.jar-not-loaded boot path).
- **Site 2 — OSR bail path (~13712):** upgraded the existing re-stash-only AIOOBE
  drain to fully mirror the adjacent NPE block: build the exception, try
  `find_exception_handler_any_pc` against the current frame at `entry_pc`; on a
  hit, clear the stack, push the exc, set `frame.pc = handler_pc`, fire JVMTI
  catch, and `return None` (resume in the catch block). On no-handler or
  build-failure, re-stash via `stash_jit_pending_aioobe(index, length)` (OSR
  return type has no error channel) so the exception survives the
  OSR→interpreter handoff.
- **Site 3 — cached-JIT call path (~15262):** drained on the normal-return path
  (above the `i64::MIN` branch) and routed through
  `route_jit_exception_through_method(shared, thread, frame_idx, cached,
  usize::MAX, exc)`, identical shape to the NPE block directly above. The old
  in-`i64::MIN`-arm AIOOBE drain (which routed an *uncatchable*
  `InternalError(VmError::Runtime(..))`) is now superseded by this catchable
  routing and left in place as a harmless defensive belt (its `take` will be
  `None` since the new drain consumes the flag on every return path); a comment
  records this.

## Files touched
- `vm/src/runtime/interpreter.rs` — three drain sites (early-compile, OSR bail,
  cached-JIT call). No other files modified.

## Tests added
None added in this change. Rationale: the drain logic lives deep inside private
functions requiring a full `SharedVm`/`JvmThread` and a JIT'd frame to exercise,
and the helper-level flag round-trip + AIOOBE-set paths are already covered by
`vm/src/jit/helpers.rs` tests (`jit_*store`/`jit_*load` OOB → `Some((idx,len))`,
lines ~3528-3626). A format-only test would merely duplicate
`vm/src/runtime/alloc_fastpath.rs`'s existing `"Index N out of bounds for length
M"` assertion. Per the task's "add a test only if confident it compiles", no
VM-scaffolded test was added to avoid a fragile/incorrect test.

## Follow-up & risk
- Risk: LOW. The new drains are gated by `if let Some(..) = take_jit_pending_aioobe()`;
  on the common (in-bounds) return the take yields `None` and the block is a
  no-op (one TLS `Option` take, same cost as the adjacent NPE drain) — the
  in-bounds fast path is unchanged. Behavior change is strictly an improvement:
  a JIT void-store OOB now throws a *catchable* `ArrayIndexOutOfBoundsException`
  routed through the method's own exception table instead of leaking/0-fabricating.
- Reachability nuance (from the report's B1): current `jit/src/x64.rs` emits an
  inline `emit_bounds_check` (the load helpers' silent-OOB arm is presently a
  latent fallback), but `jit_bastore`'s OOB arm is one codegen change from live;
  this drain ensures any helper that *does* set the flag is surfaced correctly.
- Suggested follow-up (needs a file I do not own): add a `vm/tests/` integration
  test that JIT-compiles a method doing an out-of-bounds void array store inside
  a `try { ... } catch (ArrayIndexOutOfBoundsException e) { ... }` and asserts the
  catch runs and `e.getMessage()` == `"Index <i> out of bounds for length <n>"`.
  Also consider the report's Feature Suggestion #1 (a single typed `JitFault`
  drain) to make the silent-fabrication class structurally impossible.
