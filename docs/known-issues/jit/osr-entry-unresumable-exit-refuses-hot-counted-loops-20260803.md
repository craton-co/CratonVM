# `osr-entry-unresumable-exit` — one artifact-wide veto, two causes; one fixed

**Status:** PARTLY FIXED 2026-08-03. A hot **call-free** counted loop in a
rarely-invoked method now leaves the interpreter (136x on the reproduction
below). A loop containing a **call** still may not, for a second and different
reason, characterized at the bottom of this doc.

## The rule that makes one bad snapshot fatal

`CompiledMethod::osr_exit_policy` is an **artifact-wide veto**: it walks *every*
deopt point in the artifact and, if any one of them reconstructs a frame that
cannot be resumed, refuses OSR entry at **every** pc of that method. That is the
correct rule — entering, committing loop iterations, then bailing somewhere
unresumable is the replay bug the whole OSR-exit design exists to prevent — but
it means one unresumable point anywhere disables OSR everywhere in the method.

Both causes below are instances of that: a snapshot that had no business
existing, and a snapshot that cannot be described.

## Cause 1 — a map at every bytecode boundary (FIXED)

`deopt-osr` Step 7 emits an OSR-exit map "at a loop-boundary bci". It did not.
The condition it sat under was `pc < osr_entry_native.len()` minus the
LICM-hoisted interiors — i.e. essentially every pc — so `StaticFieldProbe.control`
recorded fourteen maps:

```
OSR-exit map emitted at bci=0 (locals=4, stack=0)
OSR-exit map emitted at bci=1 (locals=4, stack=1)   <-- mid-expression
OSR-exit map emitted at bci=2 (locals=4, stack=0)
...
```

A mid-expression map has a partially-built operand stack, and an operand stack
entry is unresumable in any method that touches a `long`/`float`/`double`: the
stack has no per-entry width source, so `build_and_record_deopt_point` records a
non-oop entry as `FrameValue::Unsupported` rather than risk a truncated `long`
on resume. bci 1 of `control` is `lstore_1`, one `long` on the stack — and that
one entry vetoed the whole method.

Every counted loop in every method with a `long` accumulator was refused, naming
whichever bci first had a non-empty stack, usually bci 1:

```
OSR-refuse StaticFieldProbe.staticMutInt(I)J entry_pc=4 JIT bailout
  [unsupported_shape]: osr-entry-unresumable-exit
  (deopt point at bci 1 (OsrExit) reconstructs an unresumable frame)
```

**Fixed** by emitting the map only at back-edge targets — which is where the
metadata is ever consulted (`osr_entry_frame_state(entry_pc)` looks it up at the
entry bci, and the only reason-7 stub emitter is the Step-8 test trigger, which
picks the lowest loop header). The header set is derived from the same `code`
slice the walk iterates, so it stays in the walk's coordinate space when the
bytecode loop rewriter is armed.

`control` now records exactly one map, at bci 4, `stack=0`.

### What that bought

`probes/StaticFieldProbe.java` warmed to only 200 invocations — below the
tier-up threshold, so OSR is the *only* route into compiled code. 2M iterations,
same probe, same host:

| rung | before | after | HotSpot |
|---|---|---|---|
| control (no field) | 163.04 | **1.20** | 0.76 |
| `static final` REF | 750.32 | **2.52** | 1.15 |
| `static final` REF hoisted | 627.19 | **2.47** | 1.23 |
| instance field | — | **2.06** | 1.23 |

136x on the control rung, and the checksum matches HotSpot
(`sink=-1428027462866001286`). `VirtOnlyProbe.arith` moves 139.50 → 1.32 the
same way.

Regression test: `osr_exit_maps_are_emitted_at_loop_headers_only`
(`jit/src/x64/tests.rs`) — with the gate reverted it fails with
`left: [0, 1, 2, 3, 5, 6]`, `right: [2]`.

## Cause 2 — a call-site guard whose operand stack cannot be typed (OPEN)

With cause 1 gone, **every** remaining refusal across the regression-suite
corpus is a `ReceiverTypeChanged` guard — the call-site type-check snapshot,
taken before the argument pops, so the operand stack still holds
`[.., receiver, args]` — and in every case exactly one stack entry blocks it:

```
RJitGc.main entry_pc=7 (deopt point at bci 57 (ReceiverTypeChanged)
  reconstructs an unresumable frame: stack 0 (Unsupported) of 1)
```

bci 57 is `invokestatic Double.doubleToLongBits(D)J`. The blocking entry is the
`double` argument. Note the shape of the veto: bci 57 is *outside* the loop
being entered, and it still refuses entry at pcs 7, 85, 152, 198, 209 and 248.

Distribution of the blocking entry across six corpus classes, before any of the
work below:

| blocking entry | count |
|---|---|
| `stack 0 of 1` | 30 |
| `stack 1 of 2` | 27 |
| `stack 1 of 3` | 12 |

Shallow stacks, blocking entry at or near the top: these are call arguments.

**Partial fix applied.** The argument tags were already being derived from the
descriptor — but only for `invokedynamic` (`indy_stack_arg_types`, added for an
unrelated trap). That is now generalized to every invoke carried in
`invoke_info` (`invoke_stack_arg_types`), with an alignment cross-check: the
tags are positional, so every tagged entry must be a ref exactly when its oop
mark says ref, or the whole vector is discarded for that bci. A misaligned
vector would type the *wrong* entries — a truncated `long` on resume, which is
the corruption the coarse fallback exists to avoid — and the oop marks are an
independent per-entry opinion the emitter already maintains for the GC.

That moves `RJitGc.main`'s first blocker from bci 57 to bci 67 and takes
`RMapGcStress` from 39 refusals to 37. It does not close the class.

### What is actually left

bci 67 is `invokestatic make:(II)LRJitGc$Tree;` — a same-class static, so its
metadata is a `JitDirectCall`, which carries `num_params` and `return_type` but
**no descriptor**, so there are no per-argument tags to align. The same is true
of intrinsic call sites and of MIC/PIC slots. Each metadata kind is a separate
plumbing job, and *any* missed source leaves the artifact-wide veto in place.

Three ways to close it, cheapest first:

1. **Add `descriptor: &'static str` to `JitDirectCall`** and index it exactly as
   `invoke_stack_arg_types` does. ~12 producer sites (`vm/src/runtime/
   interpreter.rs`, `interpreter/invoke.rs`, `jit/src/lib.rs`), each with the
   descriptor already in scope; the `&'static str` follows `JitInvokeInfo`'s
   existing leak/intern pattern. Closes direct calls; leaves intrinsics.
2. **Give the operand stack a real type model.** A forward dataflow over the
   bytecode producing a per-bci stack-kind vector, consulted by
   `build_and_record_deopt_point`. This subsumes every call-metadata source —
   it needs only *arity* and *return type* at a call (both already present on
   `JitInvokeInfo` AND `JitDirectCall`), because argument types come from
   wherever the values were pushed. It is the real fix and the largest one:
   ~200 opcodes of typed stack effects, a merge rule at joins, and handler
   entry states. Make it strictly additive — it may only ever upgrade
   `Unsupported` to a precise tag, never the reverse — and guard the consult
   site with an analysis-depth == emitter-depth equality check, so a modelling
   error that changes depth degrades to today's behaviour instead of
   mistyping a slot.
3. **Track kinds in the emitter's abstract stack** (a `stack_kinds` vector
   maintained in `push_stack`/`stack_push`/`pop_stack`, mirroring
   `stack_oop_marks`). The choke points exist and the desync hazard is already
   solved for oop marks — but ~23 push sites have to set the right kind, and a
   wrong one is silent corruption. Option 2 is the same information with the
   correctness argument in one place instead of twenty-three.

Do **not** close it by weakening `osr_exit_policy`. The veto is doing its job;
what is missing is the ability to describe the frame.

## Reproduction

```bash
# Cause 1 (fixed): a probe warmed by ITERATIONS, not invocations, so OSR is the
# only route into compiled code. The control rung tells you which tier ran.
cratonvm --java-home <jdk25> -cp <probes> StaticFieldProbe 2000000

# Cause 2 (open): every refusal names its blocking slot.
CRATONVM_DBG=jitc cratonvm --java-home <jdk25> -cp regression-suite/build RJitGc \
  2>&1 | grep 'unresumable frame'
```

`probes/StaticFieldProbe.java` and `probes/VirtOnlyProbe.java` now warm 1200
invocations each so they measure compiled code by the ordinary tier-up path;
temporarily lowering that to 200 is what isolates OSR.

## Where to look

* `jit/src/x64/bytecode_walk.rs` — the Step-7 emission site and its header gate.
* `jit/src/lib.rs` — `CompiledMethod::osr_exit_policy`, `osr_entry_frame_state`,
  `OSR_REFUSE_UNRESUMABLE_EXIT`.
* `jit/src/deopt.rs` — `frame_state_is_resumable`, `first_unresumable_slot`
  (the refusal's slot naming).
* `jit/src/x64/deopt_stubs.rs` — `build_and_record_deopt_point`'s operand-stack
  loop: the `wide_fp` fallback and the argument-tag path.
* `docs/jit/on-stack-replacement.md` — the refusal taxonomy.
