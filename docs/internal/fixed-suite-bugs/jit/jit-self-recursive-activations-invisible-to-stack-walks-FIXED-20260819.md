# A JIT-compiled method's self-recursive activations are invisible to every Java stack walk — FIXED

**Status: CLOSED 2026-08-19.** All three mechanisms this page tracked are fixed
and measured against HotSpot 25 on the Azure Linux host. The suite test it was
opened for, `wp1_9_stackwalker::stackwalker_log4j_deep_repeated_walks_finish_under_jit`,
passes 10 runs out of 10; its probe passes 20 out of 20 where it previously
failed 10 out of 12 of the SAME binary.

Retired from `docs/known-issues/jit/jit-self-recursive-activations-invisible-to-stack-walks-20260818.md`.
Branch `fix/jit-stackwalk-cross-chain-frames-20260819`, off `dev` `974471245`.

One thing this page reported as open turned out **not to be a stack-walk defect
at all**, and is split out to `docs/known-issues/jit/jit-eliminates-self-tail-call-frames-20260819.md` —
see "Split out" below before reading the history.

## The three mechanisms, and where each was closed

| # | mechanism | closed |
|---|---|---|
| 1 | `JIT_ENTRY_CHAIN` holds one entry per interpreter→JIT boundary, so 64 nested activations of a self-recursive method reported as ONE frame | 2026-08-18, `f5e941a4b` — `active_compiled_frames` walks the saved-RBP chain |
| 2 | the innermost compiled frame of every fast-tier method was reported under its CALLER's name | 2026-08-19 — `cm.compile_id = compiler.compile_id;` in `x64/driver.rs` |
| 3 | a compiled frame entered by a direct compiled→compiled call was **dropped entirely** | 2026-08-19, this branch — see below |

Plus one residual this page recorded as an aside, also closed here: an OSR'd
method appeared TWICE in every trace.

## Mechanism 3 — the direct call has TWO encodings and only one decoded

### What was actually wrong

`x64::emit::emit_call_absolute` emits one call to one known address in either
of two forms, and picks between them by arithmetic done at emit time:

```rust
let delta = (addr as i128) - (next_pc as i128);
if delta >= i32::MIN as i128 && delta <= i32::MAX as i128 {
    // E8 rel32 — 5 bytes
} else {
    self.emit_call_imm64_via_rax(addr);   // 48 B8 <imm64> FF D0 — 12 bytes
}
```

`conservative_roots::direct_call_target` decoded only the first. So
`innermost_frame_method` — which falls back to decoding the call that built a
frame when the frame-record mirror cannot name it — answered `None` for every
frame built by the second form, and `active_compiled_frames` then reported that
frame's ancestors while dropping the frame itself.

`next_pc` is a position in the emit-time buffer, not the final code address, so
which encoding a given call site gets depends on where the allocator happened to
put things on that run. **That is the whole explanation of the flakiness**: the
same binary, running the same probe, failed 10 times in 12.

### Measured, on a failing run

The chain dump (`CRATONVM_DBG_SWCHAIN=1`, added on this branch) on
`StackWalkerLog4jStressProbe`:

```
[swchain] e0 entry_sp=0x75ed5adb40ae exact_rbp=0x75ed5adb0e50 cm_id=0 depth=1
          boundary=…StackWalkerLog4jStressProbe.recurse:(I)Ljava/lang/Class;
[swchain] e0 frame@exact_rbp: [rbp]=0x75ed5adb0f10 ret=0x75ed5b9c122d
          ret_owner=…recurse:(I)Ljava/lang/Class; owner_is_boundary=true decode=<no-decode>
[swchain] e0 bytes-before-ret: 58 48 8b bd f0 ff ff ff 48 b8 00 70 9c 5b ed 75 00 00 ff d0
[swchain] e0 innermost=<none>
[swchain] e0 nested=65
```

Reading the bytes: `pop rax` / `mov rdi,[rbp-0x10]` / **`movabs rax,
0x75ed5b9c7000`** / **`call rax`**. `0x75ed5b9c7000` is
`LoggerFactory.resolveCaller`'s registered entry point. The frame at
`exact_rbp` IS `resolveCaller`; the mirror named it correctly and the decoder
could not confirm it, so it was dropped and the walk reported only the 65
`recurse` ancestors above it. 67 frames where HotSpot has 68, and Log4j2's
caller lookup then answered `StackWalkerLog4jStressProbe` instead of
`StackWalkerLog4jStressProbe$LoggerFactory`.

### The fix

`direct_call_target` now tries both encodings — `direct_call_target_rel32` then
`direct_call_target_abs64`. The absolute form is a *stronger* decode than the
relative one: the target is a literal in the instruction stream rather than a
displacement to add. Every caller still requires the decoded address to equal a
registered method's ENTRY POINT (`direct_call_callee`), so the fail-closed
property is unchanged — an inline-cache `CALL R11`, the megamorphic stub, and a
bare `FF D0` whose `MOVABS` is missing all still decode to nothing and stay
foreign. `returned_from_direct_self_call` was rewritten on top of the same
decoder, so a recursive activation is no longer called foreign on the runs where
its self-call came out absolute.

Unit-tested in `conservative_roots::tests::direct_call_target_decodes_the_movabs_rax_call_rax_form`,
including the three rejections (too close to the entry, a bare `CALL RAX`, and a
`MOVABS` followed by `JMP RAX` instead).

## The OSR residual — a method reported twice

This page recorded: "`SWCross` reports 69 frames where HotSpot reports 68,
because an OSR'd `main` appears twice — once as its interpreter frame and once
as its compiled chain entry."

Confirmed exactly:

```
tail[67] cratonvm.SWCross2.main(SWCross2.java)      <- the chain entry
tail[68] cratonvm.SWCross2.main(SWCross2.java:11)   <- the interpreter Frame
```

Cause: an ordinary JIT call is a NEW Java activation with no `Frame` of its own,
which is the whole reason `capture_full_trace` splices the chain in. OSR is the
opposite — the interpreter was already running the method, its `Frame` is still
on `thread.frames`, and the compiled body took over the SAME activation
mid-loop. Both were reported.

Fix: `JitFrameChainEntry` gained `osr_resumes_interp_frame`, set only by
`try_osr` through the new `JitEntryGuard::enter_with_osr_compiled_at`.
`active_compiled_frames` suppresses that entry's own BOUNDARY frame and nothing
else — activations nested below an OSR frame are still new frames and are still
reported, and the GC root scan is untouched (the compiled frame's spill slots
are live either way).

## Measured, after

Azure Linux, real JDK 25 backend, one release binary per column.

| probe | CratonVM before | CratonVM after | HotSpot 25 |
|---|---|---|---|
| `StackWalkerLog4jStressProbe` | 2 of 12 runs pass | **20 of 20** | 20 of 20 |
| `wp1_9_stackwalker` (whole file) | 1 test red | **10 of 10 runs, 8 passed 0 failed** | — |
| `SWFrames` walk#0 / #31 | 68 | 68 | 68 |
| `SWCross` round#39 | 69 | **68** | 68 |
| `SWShape` tail / nonTail / chain | 67 / 67 / 10 | 67 / 67 / 10 | 67 / 67 / 10 |
| `SWMutual` | 67 | 67 | 67 |
| `SWValue` | `VALUES_OK` | `VALUES_OK` | `VALUES_OK` |

`cargo test -p cratonvm-vm --release --lib`: **2572 passed, 0 failed**.

Both fixes are A/B-able inside one binary:

* `CRATONVM_JIT_NO_OSR_FRAME_DEDUP=1` restores the doubled OSR frame
  (`SWCross` round#39 goes back to 69, measured).
* `CRATONVM_JIT_NO_NESTED_TRACE_FRAMES=1` still restores the pre-`f5e941a4b`
  one-frame-per-chain-entry answer.

The decoder fix has no kill switch: it is a strictly-more-complete decode of the
emitter's own output, and a switch that turned it off would only reinstate the
allocator-dependent flake.

## Diagnostic

`CRATONVM_DBG_SWCHAIN=1` dumps, per chain entry, the recorded `entry_sp` /
`exact_rbp` / published compile id, the boundary method, whether the entry is an
OSR resumption, and the walked activation list run-length-encoded. That dump is
what identified both defects above, and the byte-dump technique that finished
the job is worth repeating: when `decode=<no-decode>` appears on a frame the
mirror named, print the twenty bytes before the return address and read the
instruction — the answer was in them.

## Split out — NOT a stack-walk defect

While verifying, `SWFrames` was found to report `n=4` instead of `n=68` in
roughly a third of runs, and it does so with **every** fix on this branch
disabled. It is not frames being missed by the walk: the frames are not there.

```
depth=4000000 self-tail-recursion  ->  CratonVM JIT: RETURNED
                                       CratonVM --nojit: STACK_OVERFLOW
                                       HotSpot 25:       STACK_OVERFLOW
```

The compiled path performs tail-call elimination on direct self-recursive calls,
so the walk is faithfully reporting a stack that genuinely holds one activation.

The emitter has since been identified: the tail-JMP arm of the self-recursive
`invokestatic` case in `jit/src/x64/bytecode_walk.rs`, which is a C1/fast-tier
lowering only — the optimizing tier emits a real `CALL` and the frames come
back. That is also why the collapse was intermittent: it is the window between
the two compiles.

That is a separate defect with a separate mechanism and its own blast radius
(`StackOverflowError` depth accounting, every caller-sensitive API), and it is
tracked in
`docs/known-issues/jit/jit-eliminates-self-tail-call-frames-20260819.md`. It is
also the answer to this page's old open question "which emitter is responsible
for the pre-fix collapse".

## Corrections to the retired page

* Its "What remains OPEN" section attributed the missing frame to inlining. That
  was refuted on 2026-08-19 by its own author, and the refutation was right: the
  method is fully compiled, with a real frame, entered by a direct
  compiled→compiled call. The refutation's next step — "`direct_call_callee` is
  the intended mechanism and returns `<no-decode>` here — start there" — was
  also right, and is exactly where the defect was.
* Its closing line, "Log4j2's caller lookup wants `LoggerFactory.resolveCaller`,
  which is inlined away", contradicted the refutation three paragraphs above it
  and is simply wrong. Nothing is inlined; `grep -c inline-splice` = 0 on this
  probe, as the refutation already recorded.
* Its account of the shape — "`getCallerClass` has its own chain entry … and
  `resolveCaller` … falls between two entries" — does not match the measured
  chain, which has ONE entry (`boundary=recurse`) whose `exact_rbp` names
  `resolveCaller` directly. There was never a gap between two entries.
* Its design survey costed three ways to publish a call-site PC so an extent
  table could be read. None was needed. The information was already in the
  instruction stream.

## Related

- `vm/src/jit/conservative_roots.rs` — `direct_call_target`,
  `direct_call_target_abs64`, `innermost_frame_method`,
  `active_compiled_frames`, `JitFrameChainEntry::osr_resumes_interp_frame`.
- `jit/src/x64/emit.rs` — `emit_call_absolute` / `emit_call_imm64_via_rax`, the
  two encodings.
- `vm/src/runtime/interpreter/jit_bridge.rs` — `try_osr`, the only caller of
  `enter_with_osr_compiled_at`.
- `docs/known-issues/jit/jit-eliminates-self-tail-call-frames-20260819.md` — the
  split-out finding.
