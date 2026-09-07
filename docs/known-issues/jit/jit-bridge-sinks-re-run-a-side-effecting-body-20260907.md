# The two `jit_bridge` deopt sinks re-run a side-effecting body from entry, silently

| | |
|---|---|
| **Status** | OPEN, NARROWED 2026-09-07. **Witnessed** for the site-trap half, and that half is closed at the compiler; still open for traps from an IR deopt guard. See the update below. |
| **Severity** | Potentially a silent wrong answer (a duplicated side effect), which is worse than the loud abort the sibling sink used to raise. Unquantified, because nothing has been measured to fire it. |

## 2026-09-07, later the same day — WITNESSED, and the site-trap half is closed

**Read this first; everything below it is the original page, unchanged.**

The hazard is real. The witness this page asked for exists, and it is a silent
wrong answer, exactly as predicted.

### The witness

`probes/IndySiteTrapSinkProbe2.java`, and the Rust test that drives it,
`vm/tests/jit_site_trap_never_duplicates_a_side_effect.rs`. Same binary, same
environment, one variable:

| arm | `sink=` | expected | exit | `jit-callsite` sinks fired |
|---|---:|---:|---|---:|
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | **200 241** | 200 000 | AssertionError | **241**, all `jit-callsite-a` |
| default | 200 000 | 200 000 | OK | 0 |

241 duplicated side effects in 200 000 calls, with `CRATONVM_DBG_DEOPT=1`
naming the consuming sink on every one of them. Nothing in the run says
anything is wrong — which is this page's whole point.

### And it was not rare

This page opens with *"Unquantified, because nothing has been measured to fire
it"*. It is quantified now, and the number is large. On the 56-class Spring
Framework cluster of 2026-09-07, on a binary carrying the sibling sink fix, the
only variable being whether the site trap is planted:

| arm | OK | FAIL | TIMEOUT | test-methods failed |
|---|---:|---:|---:|---:|
| trap refused (default) | 53 | 1 | 2 | 6 of 1666 |
| `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` | 34 | **21** | 1 | **302** of 1670 |

Twenty classes, 296 test methods. Confirmed ABBA-interleaved over six of them
because the pair above ran sequentially on a shared host — 315/0 passed/failed
in both default slots, 133/182 in both off slots, byte-identical between
repeats of each arm, so this is deterministic rather than load.

The abort the sibling sink used to raise was the loud minority of this defect.
The silent replay through these two sinks was the rest of it.

### Wall 4, and why the original probe could not get here

`probes/IndySiteTrapSinkProbe.java` ends at *"the door this path takes supplies
no resolver"*. That reading is wrong, and the real answer is one line away: the
`indy_trap_sites` population sits inside `if call_eligible { … }` inside
`if let Some(resolver) = cp_invoke_resolver { … }` — the **invoke-planning
block**. A method whose only call is the `invokedynamic` itself is never
invoke-planned, so the map stays empty and the 0xba arm bails on the
`indy_trap_sites.get(&pc)` miss no matter which door compiled it.

The fix to the probe is to give `hot` one ordinary `invokestatic` before the
concat. It is then invoke-planned, the resolver is asked, and `[ir] site TRAP
planted at bytecode pc 17` appears. Walls 1–3 as documented on the original
probe still apply and are unchanged.

### What is closed, and what is not

Closed: **a site trap** (`IrBuilder::plant_uncommon_trap`, the
`invokedynamic` / unresolved-class arms) is no longer planted where taking it
would re-run a committed side effect. `IrBuilder::trap_replay_is_safe` asks
`replay_from_entry_is_observably_equivalent`'s own rule at the producing end,
and a refusal bails the IR build, so the method keeps its single-pass body and
these sinks never see such a frame. That is the compiler-side strategy this
page's *"Why it was not fixed at the same time"* section names as the
alternative to touching the hot sinks, and it leaves them untouched.

Still open, and this page stays for it: the same sinks re-run from entry for a
trap from an **IR deopt guard** — `emit_array_null_bounds_guards`,
`emit_deopt_if_zero` — which the site-trap refusal does not cover. Those guards
are conditional rather than unconditional, so the hazard needs a body whose
guard actually fires after a committed side effect; no witness for that half
yet. The reasoning in the original page below applies to it verbatim.

---

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
