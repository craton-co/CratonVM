# The two `jit_bridge` deopt sinks re-run a side-effecting body from entry, silently

| | |
|---|---|
| **Status** | OPEN. Found by reading, while fixing the sibling sink; **no witness yet** — see *What would settle this*. |
| **Severity** | Potentially a silent wrong answer (a duplicated side effect), which is worse than the loud abort the sibling sink used to raise. Unquantified, because nothing has been measured to fire it. |

## The three sinks, and the three different answers

A trap taken in a compiled body stashes a reconstructed frame
(`ir_deopt_entry` → `LAST_DEOPT`) and returns the `i64::MIN` sentinel. Which
sink consumes that stash depends only on how the callee was entered — and, as
of 2026-09-07, the three sinks give three different answers to the same event:

| sink | where | on an IR body it cannot precisely resume |
|---|---|---|
| `try_resume_trapped_callee` | `vm/src/jit/helpers.rs` | resumes precisely; on a refusal leaves the sentinel to propagate |
| `execute-first-call-tierup` | `vm/src/runtime/interpreter.rs` | resumes precisely since 2026-09-07 (`deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md`); refuses loudly when it genuinely cannot |
| `jit-callsite-a` / `jit-callsite-b` | `vm/src/runtime/interpreter/jit_bridge.rs` | **re-runs the whole method from entry**, unconditionally |

## The reading

`jit-callsite-b` (`jit_bridge.rs`, the instance-method tier-up path):

```rust
if let Some(rframe) = cratonvm_jit::deopt::take_last_deopt() {
    dbg_deopt_sink("jit-callsite-b", &rframe, "");
    if ir_deopt_resume_enabled() {                       // DEFAULT OFF
        if let Some(r) = resume_from_ir_deopt(..) { return Ok(Some(r)); }
    }
    if cratonvm_jit::deopt_real_enabled() && compiled.can_deopt_resume {
        ...                                              // false on every IR artifact
    } else if ... { despeculate_stashed_frame_method(..) }
    else { ...deoptimize(..) }
    return Ok(None);                                     // <- re-run from entry
}
```

`jit-callsite-a` is the same shape, returning `CachedCallResult::CacheMiss`
after restoring the popped arguments.

Both preconditions fail on an optimizing-tier artifact, and neither failure is
a bug on its own:

* `ir_deopt_resume_enabled` (`CRATONVM_IR_DEOPT_RESUME`) is **default OFF**,
  and its own comment says why: *"the precise resume is unvalidated against the
  full VM suite, and no production IR method emits a deopt guard yet, so
  default OFF is inert."* **That second clause is no longer true.** Production
  IR methods emit deopt guards (`emit_array_null_bounds_guards`,
  `emit_deopt_if_zero`) and plant unconditional site traps at every
  `invokedynamic` they cannot lower (`ir::ir_site_trap_enabled`, default ON) —
  which is what the 2026-09-07 cross-suite crash population was made of.
* `compiled.can_deopt_resume` is false for **every** optimizing-tier artifact
  in a production build: `ir_lower` sets it only under `CRATONVM_SCALAR_DEOPT`
  + `CRATONVM_DEOPT_REAL`.

So `return Ok(None)` is what an IR trap gets here — and `Ok(None)` means
"re-execute this method from bci 0". For a body that already committed a store,
a call or a monitor action before the trap, that duplicates it. No
`replay_from_entry_is_observably_equivalent` check guards either site; the
tier-up sink's whole refusal apparatus has no counterpart here.

## Why this is not the bug that was just fixed

`deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md` fixed the
sink that **aborted**. These two do the opposite, and the abort is the reason
this was never seen: the crash population reached the tier-up sink, where the
VM said so loudly. A duplicated side effect here says nothing at all.

## What would settle this

A witness, and it has to be a witness for THESE sinks — the tier-up sink's
`storeToNull` fixture reaches the wrong one. The shape needs:

1. an optimizing-tier body that commits a side effect and then traps
   (`probes/IndySiteTrapSinkProbe.java` is the start of one: a
   `String +` for the site trap, an array store for the side effect);
2. entry through `execute_invokevirtual_cached` / the static MIC path rather
   than through `execute` — i.e. called from ordinary bytecode after the callee
   already has an artifact, not on the invocation that installs one;
3. an observable count of the side effect, so a duplicate is visible.

`dbg_deopt_sink("jit-callsite-a"/"-b", ..)` under `CRATONVM_DBG_DEOPT=1`
already names the sink when it fires, so step 3 can be as simple as checking
whether that line appears at all on a workload whose counts are known.

## Why it was not fixed at the same time

The machinery is one gate away — `build_deopt_frame_inner` +
`resume_real_ir_deopt` are exactly what the tier-up sink now uses, and dropping
`can_deopt_resume` from the condition here would let a precise resume replace
the re-run. But these two sinks are on hot, working paths, and nothing has been
measured to fire the hazard, so the change would be a behaviour change to
working code justified by a source reading. That is the thing
`unresumable-unconditional-trap-mvmap-FIXED-20260802.md` warns about in as many
words, and the reason this is a page and not a patch.

## Related

* `docs/internal/fixed-bugs/deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md`
  — the sibling sink, fixed.
* `docs/internal/fixed-bugs/inline-trap-inside-a-protected-range-FIXED-20260818.md`
  — the compiler-side refusal that avoids needing a resume at all, for its own
  narrower shape.
