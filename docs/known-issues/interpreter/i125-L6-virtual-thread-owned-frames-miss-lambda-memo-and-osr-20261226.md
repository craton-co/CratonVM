# i125-L6: a virtual thread's owned frames miss the lambda owner memo and OSR

Interpreter round i6, wave 125, lane L6. Found in wave 124 while analysing the remounted-leaf
adapters (`docs/internal/fixed-bugs/interpreter-i116-L9-osr-return-skips-continuation-return-adapters-FIXED-20261224.md`,
"Wave 124").

## Status

Performance only. No answer is wrong: every arm of `L6W124RemountedLeafAdapters` matched HotSpot.
Both fixes are built in wave 125 behind default-off switches and are unbuilt and unmeasured. Probe:
`tools/probes/interp/L6/L6W125LambdaParkMemo.java`.

| Gap | Switch | Kind | Default |
|---|---|---|---|
| (a) lambda owner memo never filled | `CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD` | performance | OFF |
| (b) `execute`'s owned frame never offered OSR on a virtual thread | `CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME` | performance | OFF |

## Gap (a): a lambda whose impl parks never reaches the memoised cached path

### Cause

The lambda door's static arm (`lambda.rs`, `try_lambda_dispatch_inner`,
`MethodHandleKind::InvokeStatic`) has three routes:

1. the loader-faithful override (`lambda_impl_dispatch_override_driven`);
2. the memoised owner (`lambda_global_impl_owner`): `try_invoke_cached_lambda_impl`, which pushes a
   CACHED frame through `install_and_run_cached_frame`;
3. otherwise, by name: `vm::invoke_static_shared` -> `interpreter::execute`, which pushes an OWNED
   frame (`Frame::new_from_arcs`) while `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` is off (its default).

Route 3 fills the memo (`record_lambda_global_impl_owner`) only after `invoke_static_shared(..)?`
returns. When the impl parks on a virtual thread, the `ContinuationYield` leaves through the `?`. At
the remount the Rust frame is gone: the parked frame resumes from `resume_continuation` and returns
through the continuation's adapters. So a lambda whose impl parks on every call never fills the
memo. Every call takes route 3: a by-name lookup (the ~2.3-3 us the memo was added to remove), and
an owned frame that the virtual thread's OSR offer refuses (gap (b)).

### Fix (built, `CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD`, default OFF)

Record on the yield path, not before the call. Recording before the call would memoise a name
whose resolution or `<clinit>` then fails. The record-after-return comment forbids that.

`record_lambda_global_impl_owner_on_yield` runs only when route 3's result is
`Err(InternalError(ContinuationYield))`. A yield proves less than a return does: it may come from
inside the declaring class's own `<clinit>`. So the record is filed only when the named class is
fully INITIALIZED (`vm::is_class_initialized_via_manager`). By JVMS §5.5 that implies its
superclasses are initialized too, so the declaring class of an inherited static is initialized as
well. (`C::m` never names an inherited interface static.) Under that condition the memo holds
exactly what a returning call would have filed:

- The current call is unchanged. It resumes in its own frame and returns through the adapter that
  `try_lambda_dispatch_noted` recorded.
- Later calls take route 2, which gives the same answer as route 3. That is the memo's existing
  contract.

The switch is the per-VM `LambdaSwitches::owner_memo_on_yield` (`runtime_flag_on`). With it off,
the only cost is one bool load on a yield.

## Gap (b): `execute`'s frames are never offered OSR on a virtual thread

### Cause

`try_osr_offer` (`interpreter.rs`) judges a virtual thread's frame through
`vthread_may_osr_method(.., cached_method())`. An `Owned` frame answers `None`, so the offer ends
in `Skip`. On a virtual thread, every frame pushed by `interpreter::execute` is owned: by-name
lambda impls (gap (a)), reflective calls, native up-calls, and `run` itself. None of their loops
can be OSR'd while they run on a virtual thread.

### Is `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME=1` safe on virtual threads? Yes, by reading

- **JDK continuations (the default path).** A yield does not move frames. `execute` returns the
  `ContinuationYield` without popping (`interpreter.rs`, the yield arm before
  `pop_root_frames_keeping_result`). The frames stay in `thread.frames`. `resume_continuation` runs
  them from index 0 with `execute_frame_from_index`, exactly as it runs the interpreter's own cached
  frames, which virtual threads push on every interpreted invoke. The remount does not care about
  the frame kind.
- **`Frame::new_cached_unpooled`** draws its locals and operand stack from the per-OS-thread pools,
  as `new_from_arcs` does. A frame that pops on another carrier is therefore no different from an
  owned frame. Its record (`JitRealm::execute_reentry_records`) is a per-VM `Arc`, so it lives
  across carriers.
- **Redefinition** is handled as for any frame: `obsolete_frames::convert_thawed_frames` at the
  remount and `frame_runs_replaced_code` in the OSR offer.
- **The legacy continuation path** (`Continuation::freeze_frames` / `thaw_frames`) rebuilds every
  thawed frame as owned, the interpreter's cached frames included. A cached frame gains nothing
  there, but it costs nothing in correctness either, and `vthread_osr_door_on` requires a JDK
  continuation anyway.
- **Measured once.** On w122 (`61b15a25bb`), `L6W124RemountedLeafAdapters` with the switch on
  (`--jdk-only`) printed stdout equal to HotSpot, with 6 adapters, all converted.

### Fix (built, `CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME`, default OFF)

This is the contained virtual-thread-only gate. `execute` computes
`execute_cached_reentry_frame || (vthread_execute_cached_frame && thread.kind == Virtual)` once and
uses it where it used the global switch alone. Platform threads keep the owned frame. With the
switch off, the only cost is one bool load per `execute`. The switch is `VmConfig::vthread_execute_cached_frame`.

### What a flip of the global switch would need (a separate decision; not made here)

`CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` was retired KEPT-OFF in round i3 (page
`interpreter-i77-L4-proposal-a-native-re-entry-pushes-a-cached-frame-KEPT-OFF-20261111.md`). The
behaviour was the same, and no `L7W79ReentryFrameBench` row gained, because the frame build is a
small share of `execute`'s fixed cost. The virtual-thread gain above does not change that verdict
for platform threads. A global flip would need:

1. a platform-thread win, or at least no loss, on `L7W79ReentryFrameBench` and the reflective-door
   census (`CRATONVM_REFLECT_DOOR_PHASE_STATS=1`), both modes;
2. the full interpreter battery and the virtual-thread probes (`L6W1xx`, `L9W116`) SAME with it on,
   in both modes and with `--nojit`;
3. a look at the growth of `execute_reentry_records` (one row per re-entered method) on a
   class-heavy run, such as the Spring Boot sample. This lane did not read its pruning.

The virtual-thread gate needs only (2), restricted to virtual-thread probes, plus a gain row on
`L6W125LambdaParkMemo`.

## Verification (orchestrator)

```
javac -d . tools/probes/interp/L6/L6W125LambdaParkMemo.java
CRATONVM_DBG=jit-compiled cratonvm --jdk-only -cp . L6W125LambdaParkMemo                                           # OFF
CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD=1 CRATONVM_DBG=jit-compiled cratonvm --jdk-only -cp . L6W125LambdaParkMemo     # A
CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME=1 CRATONVM_DBG=jit-compiled cratonvm --jdk-only -cp . L6W125LambdaParkMemo   # B
```

Repeat each command with `--compatible`, and with `--nojit` (stdout only).

- **Every arm:** stdout without the `TIME` lines equals the HotSpot block.
- **Gain (A, B):** an `ir-osr` line names `parkLeaf()I`. OFF has none.
  `TIME loop-median-late-us` in A and B is well below OFF. For reference, HotSpot 25.0.3 on this
  host measured about 310 us with the JIT and about 2850 us with `-Xint`.
- If A shows no gain but B does, the memo did not fill. Check `CRATONVM_DBG_LAMBDA=1` for
  `static-pre-invoke` lines. A switch that shows no gain stays OFF.

Retire this page to `docs/internal/` once A or B shows the gain, or record KEPT-OFF here if neither
does.
