# `CRATONVM_JIT_IR_INLINE` turned an `IndexOutOfBoundsException` into an `InternalError`

## Status

**FIXED, 2026-08-28.** `io.netty.buffer.DuplicatedByteBufTest` is
`ok=416 failed=0` with the flag on — **3 reps**, serial, against `ok=415
failed=1` on the same binary's predecessor and the same three reps with the flag
off. The correctness regression that blocked the flag's default-on flip is gone.

| arm | before | after |
|---|---|---|
| `CRATONVM_JIT_IR_INLINE` unset | `ok=416 failed=0` | `ok=416 failed=0` ×3 |
| `CRATONVM_JIT_IR_INLINE=1` | `ok=415 **failed=1**` | **`ok=416 failed=0`** ×3 |

The page's reading of the error was reasonable and wrong in one specific way,
and that way is the interesting part: **the reason it names was never
requested.**

## Two defects, one behind the other

### 1. The spliced-bci argument was dropped on the floor

`ir_lower::lower_inner_with_scopes` takes a `spliced_ranges: &[(usize, usize,
usize)]` — `(start, end, invoke_bci)` per relocated callee body — and hands
`Lowerer::new` a literal `&[]`. So `Lowerer::resume_bci`, whose entire job is to
map a bci inside a spliced body back to the enclosing `invoke`, was **the
identity in every production lowering**. The argument arrived from `lib.rs`
correctly and went nowhere.

The `[ir-graph]` dump of the failing method says it plainly:

```
[ir-graph] io/netty/buffer/UnpooledHeapByteBuf._getUnsignedMedium(I)I — 26 node(s)
[ir-graph]     6: Load(Ref)          bci=Some(1)
[ir-graph]     7: ArrayLoad(Byte)    bci=Some(11)
[ir-graph]    14: ArrayLoad(Byte)    bci=Some(23)
[ir-graph]    21: ArrayLoad(Byte)    bci=Some(36)   <- the out-of-bounds read
[ir-graph]   safepoint[0] bci=0   safepoint[1] bci=1   safepoint[2] bci=4
[ir-graph]   safepoint[3] bci=5   safepoint[4] bci=8
```

`_getUnsignedMedium` is **9 bytes** (`aload_0; getfield array; iload_1;
invokestatic; ireturn`) and the spliced `HeapByteBufUtil.getUnsignedMedium` is
**34**, so the relocated body occupies bcis 9–42 and the third `baload` — the
one the test drives past the end — sits at **36**. The safepoints stop at 8.
Bci 36 is not a program point of the method, and no snapshot describes it.

Everything in the message follows from that:

* `at bci 36` — the raw relocated bci, because `resume_bci` never ran.
* `reason UnreachedCode` — **a default, not a trap.** The interpreter looks the
  reason up as `deopt_points.iter().find(|dp| dp.bci == rframe.bci)` and
  `unwrap_or(UnreachedCode)`. No point carries bci 36, so the sink invented the
  reason the page then spent its analysis on. The real deopt was the array
  bounds check, `DeoptReason::BoundsCheck`, exactly as designed.
* the otherwise-empty frame with a stamped method key — `resolve_frame_state_for_bci(36)`
  found no safepoint and returned the empty `FrameState`; `stamp_deopt_method_key`
  filled the name in afterwards.

The page's "the spliced body has a live path that requests a deopt, and the
artifact it lives in cannot service one" was right about the symptom. The path
is an ordinary bounds check, and what could not service it was one `&[]`.

### 2. The replay rule asked about the whole body, not about the abandoned attempt

Fixing (1) is necessary and not sufficient. `can_deopt_resume` is false for every
IR artifact in production (it is only set under `CRATONVM_SCALAR_DEOPT` +
`CRATONVM_DEOPT_REAL`), so the only route is a whole-method re-run, which the
interpreter allows when the body "commits no side effect". `_getUnsignedMedium`
contains an `invokestatic`, so the replay was refused — **even though the deopt
is at that very invoke, nothing before it commits anything, and the call never
completed.**

That is why the flag is what exposed this. With the flag off the bounds check
happens inside `getUnsignedMedium`'s own artifact, which contains no call, so
the replay is allowed and the ordinary `ArrayIndexOutOfBoundsException` comes
out. Splicing moved the deopt into a method whose bytecode has a call in it.

The rule is now a **prefix**: only `code[..resume_bci]` can have been committed,
because every deopt point this VM emits carries `ResumeSemantics::REEXECUTE`
(`PendingException` frames use a different stash), so the bytecode *at* the
resume bci had not completed and the ones after it never ran. The other half of
an abandoned spliced attempt is the relocated body, which the caller's bytecode
does not describe, so the artifact now records
`CompiledMethod::spliced_bodies_side_effect_free`, computed at compile time with
the same predicate over each spliced region.

`replay_from_entry_is_observably_equivalent(code, spliced_bodies_pure, resume_bci)`
is one function, asked at the one place, and it is strictly wider than the rule
it replaces — it answers `true` for everything the old rule did.

## Two things fixed on the way

* **`Op::Guard` was the only deopt emitter resolving from a raw bci.** Every
  other one in `ir_lower.rs` goes through `resume_bci`. `IrBuilder::splice_guard_seen`
  refuses a graph that built a guard inside a splice, so it cannot fire today —
  but a fence plus an asymmetry is one fence away from the defect above, and
  agreeing with the other emitters costs nothing.
* **The message no longer prints a reason it invented.** When no deopt point
  carries the bci it now says so, rather than naming `UnreachedCode` as though
  something had requested it. That default cost this page a day of analysis
  aimed at a trap that was never emitted.

## Evidence

* `io.netty.buffer.DuplicatedByteBufTest`, serial, flag on: `ok=416 failed=0`
  ×3 (was `ok=415 failed=1`). Flag off: `ok=416 failed=0` ×3. The two arms now
  agree, which is what the page asked for.
* `regression-suite/run.sh`: **72 passed, 0 failed**.
* `cargo test --workspace --lib --no-fail-fast` on Linux: 14 990 passed.
* Unit tests, both halves:
  * `ir_lower::tests::a_deopt_inside_a_spliced_body_resumes_at_the_enclosing_invoke`
    builds the netty shape (an `ArrayLoad` at relocated bci 36, a caller
    snapshot at the invoke it replaced at bci 5) and asserts every emitted deopt
    box carries bci 5 — with a no-ranges **control** asserting the raw 36, so the
    test states what the bug looked like as well as what the fix does.
  * Five tests for `replay_from_entry_is_observably_equivalent` over
    `_getUnsignedMedium`'s actual nine bytes: the prefix rule admits it, an
    impure spliced body vetoes it, a resume point past a `putfield` still
    refuses, a pure body answers exactly as before (including for the `u32::MAX`
    re-run sentinel), and an out-of-range bci refuses rather than answering from
    a truncated walk.

## What did not reproduce, and why that was informative

`probes/IrInlineBoundsProbe.java` reduces the shape to one file — the
three-`baload` static accessor called from a nine-byte instance method — and
**does not reproduce it**, on any of HotSpot, flag-off or flag-on. The probe is
kept because the reason is worth having written down: `_getUnsignedMedium` in
the probe never reaches C2 (`[ir] admission …: optimize=false — the C1/fast
tier was requested`), so it is never spliced, and its single-`baload` sibling
`_getByte` *does* reach C2, splice, and answer correctly. The IR inliner's
admission set allows array access (`0x2e..=0x35 … if ir_mode`) while the
single-pass one refuses it (`no!("array-load")`), so the C1 log line naming an
array-load refusal is a different resolver answering a different question.

A reproducer for a tier-dependent defect has to reach the tier, and "the same
bytecode shape" does not get you there.

## Related

- `performance/ir-inline-gauntlet-soak-20260828.md` — the soak that found it
- `docs/jit/ir-tier-inlining.md` — the design note
