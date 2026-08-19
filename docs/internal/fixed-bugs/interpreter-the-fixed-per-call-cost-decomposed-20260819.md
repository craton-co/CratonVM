# The interpreter's fixed per-call cost, decomposed — 2026-08-19

**Status:** decomposition COMPLETE and instrumented; one of the three targets
fixed (~2.4% on zero-argument calls). The two larger ones are named, measured
and OPEN.

**Instruments:** `probes/InvokeFrameCostProbe.java` (sizes the fixed cost),
`probes/ZeroArgCallProbe.java` (a workload of nothing else, for reading the
phase tally), `CRATONVM_DBG_INVOKE_PHASES=1` (the tally itself).

## Why an instrument and not another probe

`InvokeFrameCostProbe` had already established a fixed **~206ns per interpreted
call** against HotSpot's ~6.9ns — about 30x, on a host running `iadd` at only
4.1x — and had REJECTED both shape-dependent explanations: frame locals-init and
operand-stack depth each cost CratonVM proportionally LESS than HotSpot.

What remains does not depend on the callee's shape, so varying the callee cannot
subdivide it. No Java-level probe can separate the inline-cache lookup from the
frame build from the push. Reading the code produced three candidates and no way
to rank them, and ranking by reading had already cost a withdrawn claim that
week ("final dispatch is 3.8x virtual", which was host drift landing on
sequentially-ordered arms).

## The breakdown

`invokestatic` is the cleanest case — no receiver, no vtable, no inline-cache
receiver guard — and still cost 170ns. 8,000,348 zero-argument calls:

| phase | cyc/call | share |
|---|---|---|
| **frame_build** | **128.5** | **33.8%** |
| **ic_lookup** | **92.0** | **24.2%** |
| args | 60.9 | 16.0% |
| frame_push | 52.8 | 13.9% |
| guards | 45.7 | 12.0% |

**Read this as a ranking, not a costing.** `rdtsc` costs ~20-30 cycles against a
~600-cycle call, so five reads inflate the call ~20% and inflate every phase
EQUALLY in absolute terms, biasing shares toward the short phases. `frame_build`
being the largest is solid; `frame_push` at 13.9% against `guards` at 12.0% is
not a meaningful difference.

**A workload of nothing but zero-argument calls was required.** Read off
`InvokeFrameCostProbe` instead, the `args` phase aggregates its 0-, 4- and
8-argument kernels, because the counters are process-wide. That answers "what
does argument handling cost on average across this probe", not "what does a
zero-argument call cost" — which is the fixed cost under investigation.
`ZeroArgCallProbe` exists for that reading alone.

## Half the fixed cost is outside these phases

The phases sum to ~380 cyc; an uninstrumented call is ~720 cyc (240ns at ~3GHz).
Subtracting rdtsc overhead leaves the call side at roughly 255 cyc. So **about
half of the fixed per-call cost is in the callee body (two opcodes) and the
RETURN / frame-pop path**, neither of which is instrumented here. The table
above is the call side only, and saying so is the difference between a
decomposition and a partial one presented as complete.

## Target 1, FIXED: 60.9 cycles of argument handling on a call with NO arguments

`args_buf` was `[Value; MAX_INLINE_ARGS]` with `MAX_INLINE_ARGS = 16`, and
`Value` is 16 bytes (measured, `report_value_size`), so **256 bytes were
initialised on every call regardless of arity**.

This is the finding that reflects on the previous change: the 2026-08-18
argument-scan hoist fixed the per-ARGUMENT half of argument handling while a
comparable per-CALL cost sat directly beside it, unmeasured.

Fixed by giving `num_params == 0` a path that builds no buffer and scans no
descriptor at all, and by shrinking the array 16 -> 8 (which halves the
initialisation for every other call too; wider calls still spill to `args_vec`
exactly as before).

**Result: ~2.4% on zero-argument calls.** The phase share moved 16.0% -> 13.4%.

That number is much smaller than the share change alone suggests, and the
arithmetic explains why: `args` is 16% of the CALL SIDE, and the call side is
only about half the total per-call cost, so 28% x 16% x 50% is about 2.2% —
which is what was measured. **A large proportional win inside a small phase of a
partial budget is a small absolute win.**

### How that 2.4% was established, and a trap in it

The two binaries differ only by this change, but a cross-binary comparison is
not an A/B, so the number needed care. Six interleaved rounds with the order
reversed at round 4 showed a **position effect larger than the effect being
measured**: whichever binary ran SECOND was slower in 5 of 6 rounds. Comparing
like-for-like slots removes it:

| slot | OLD | NEW | |
|---|---|---|---|
| ran first | 223.2 | 217.9 | -2.4% |
| ran second | 236.5 | 231.1 | -2.3% |

Both positions agree. Reading the raw pairs instead would have given "3 worse,
1 same, 2 better" — a wash — because the position effect was assigned to the
arms.

## Target 2, OPEN: `frame_build`, 128.5 cyc (33.8%) — the largest

`size_of::<Frame>()` is **296 bytes** (measured, `report_frame_size`;
`FrameInner` alone is 80 and `ValueStack` 64). That struct is built and then
moved by value into the frame stack on every call — roughly 5 cache lines
written per invocation and read again on pop, where HotSpot's interpreter frame
is a few words on the native stack.

It also takes **two `Arc` refcount bumps per call**: `cached.clone()` at the
call site and `cached.code.clone()` inside `Frame::new_pooled_cached`. At
~20-40 cycles per uncontended atomic RMW those two plausibly account for much of
the phase — and the second is arguably redundant, since `inner` already holds
the `Arc<CachedBytecodeMethod>` that OWNS that `code`. The `code` field exists to
avoid a pointer chase per opcode, so removing it outright would trade one atomic
per call for one indirection per opcode; carrying a raw pointer kept alive by
`inner` is the shape that would win both, and it needs care rather than a quick
edit.

## Target 3, OPEN: `ic_lookup`, 92.0 cyc (24.2%)

`InvokeCache` is an `FxHashMap` keyed by `(ClassId, u16, bool)` and **clones its
entry on every hit** — a ~40-64 byte enum copy carrying 1-2 `Arc` refcount
bumps. The field, cast and `new` site caches added by the 2026-08-18 audit are
direct-mapped arrays that neither hash nor clone.

This was the structural suspicion that started the whole investigation. It is
now **confirmed as the number-two cost**, at about a quarter of the call side —
but it was worth measuring rather than acting on, because it is not the largest,
and the largest was not on the original list at all.

The clone is not semantic: it exists because the code afterwards needs
`&mut thread` while the cache entry is borrowed. A direct-mapped table returning
a small `Copy` summary, or a restructuring that drops the borrow before the
mutation, would both remove it.
