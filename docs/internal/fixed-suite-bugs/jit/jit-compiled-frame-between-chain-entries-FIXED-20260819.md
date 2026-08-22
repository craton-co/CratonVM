# ✅ FIXED — a compiled frame BETWEEN two JIT chain entries was invisible to every Java stack walk

**CLOSED 2026-08-20**, on `fix/jit-stackwalk-cross-chain-frames-20260819`. The
frame was never in a gap. It was reachable all along; the decoder that had to
name it understood only one of the two encodings the emitter uses for a direct
call, so `innermost_frame_method` answered `None` and `active_compiled_frames`
reported that frame's ancestors while dropping the frame itself.

`stackwalker_log4j_deep_repeated_walks_finish_under_jit`, which this page was
opened for, is green.

## What was actually wrong

`x64::emit::emit_call_absolute` emits one call to one known address in either of
two forms, and picks between them by arithmetic done at emit time:

```rust
let delta = (addr as i128) - (next_pc as i128);
if delta >= i32::MIN as i128 && delta <= i32::MAX as i128 {
    // E8 rel32 — 5 bytes
} else {
    self.emit_call_imm64_via_rax(addr);   // 48 B8 <imm64> FF D0 — 12 bytes
}
```

`conservative_roots::direct_call_target` decoded only the first. `next_pc` is a
position in the emit-time buffer, not the final code address, so which encoding
a given call site gets depends on where the allocator happened to put things on
that run. **That is the whole explanation of the flakiness** this page recorded:
the same binary, running the same probe, failed 10 times in 12.

### Measured, on a failing run

The chain dump (`CRATONVM_DBG_SWCHAIN=1`, added with the fix) on
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
`recurse` ancestors above it.

### The fix

`direct_call_target` now tries both encodings — `direct_call_target_rel32`, then
`direct_call_target_abs64`. The absolute form is a *stronger* decode than the
relative one: the target is a literal in the instruction stream rather than a
displacement to add. Every caller still requires the decoded address to equal a
registered method's ENTRY POINT (`direct_call_callee`), so the fail-closed
property is unchanged — an inline-cache `CALL R11`, the megamorphic stub, and a
bare `FF D0` whose `MOVABS` is missing all still decode to nothing and stay
foreign. `returned_from_direct_self_call` was rewritten on top of the same
decoder, so a recursive activation is no longer called foreign on the runs where
its self-call came out absolute.

Unit-tested in
`conservative_roots::tests::direct_call_target_decodes_the_movabs_rax_call_rax_form`,
including the three rejections (too close to the entry, a bare `CALL RAX`, and a
`MOVABS` followed by `JMP RAX` instead).

## Corrections to the page below

* **"It falls in the gap between two chain entries"** — it does not. The
  measured chain has ONE entry (`boundary=recurse`) whose `exact_rbp` names
  `resolveCaller` directly. There was never a gap, and `getCallerClass` does not
  own a chain entry of its own here.
* **The proposed redesign** — "one walk over the whole native stack instead of a
  band per chain entry", copying `remap_active_jit_frames`' Stage 5 — was not
  needed, and would have been a large change made for the wrong reason. The
  information was already in the instruction stream.
* **The three refuted hypotheses were right to be refuted**, and the refutation
  of the inlining one pointed at the answer: "`direct_call_callee` is the
  intended mechanism and returns `<no-decode>` here — start there" is exactly
  where the defect was.
* Its closing note that this was "the last of four mechanisms" holds, with one
  correction: what remained after it was not a stack-walk defect at all. See
  "Also found" below.

## Measured, after

Azure Linux, real JDK 25 backend, one release binary per column.

| probe | before | after | HotSpot 25 |
|---|---|---|---|
| `StackWalkerLog4jStressProbe` | 2 of 12 runs pass | **20 of 20** | 20 of 20 |
| `wp1_9_stackwalker` (whole file) | 1 test red | **10 of 10 runs green** | — |
| `SWFrames` walk#0 / #31 | 67 | 68 | 68 |
| `SWCross` round#39 | 68 | 68 | 68 |
| `SWShape` tail / nonTail / chain | 67 / 67 / 10 | 67 / 67 / 10 | 67 / 67 / 10 |
| `SWMutual` | 67 | 67 | 67 |
| `SWValue` | `VALUES_OK` | `VALUES_OK` | `VALUES_OK` |

The decoder fix has no kill switch: it is a strictly-more-complete decode of the
emitter's own output, and a switch that turned it off would only reinstate the
allocator-dependent flake.

## The OSR half — landed on `dev` independently

This branch also carried its own fix for the "an OSR'd method is reported twice"
residual, keyed on a `JitFrameChainEntry::osr_resumes_interp_frame` flag set by
`try_osr`. `dev` had already landed an equivalent one in `6f6a20e45` —
`stackwalker::drop_osr_continuations`, keyed on `can_osr_enter(frame.pc)`, with
its own probe and regression test. **`dev`'s is the one that ships**; this
branch's mechanism and its `CRATONVM_JIT_NO_OSR_FRAME_DEDUP` flag were dropped
at the merge, along with the near-duplicate spelling of `dev`'s
`CRATONVM_JIT_NO_OSR_FRAME_DEDUPE`. Two sessions fixed the same defect from the
same page at the same time; the merge is where that surfaced.

## Also found — NOT a stack-walk defect

While verifying, `SWFrames` was found to report `n=4` instead of `n=68` in
roughly a third of runs, and it does so with **every** fix on this branch
disabled. It is not frames being missed by the walk: the frames are not there.

```
depth=4000000 self-tail-recursion  ->  CratonVM JIT: RETURNED
                                       CratonVM --nojit: STACK_OVERFLOW
                                       HotSpot 25:       STACK_OVERFLOW
```

The single-pass backend lowers a direct self-tail-call to `JMP body_entry`, so
the activations are never pushed. Separate mechanism, separate blast radius
(`StackOverflowError` depth accounting, every caller-sensitive API), tracked in
`../../fixed-suite-bugs/jit/jit-eliminates-self-tail-call-frames-FIXED-20260820.md` and
still OPEN.

## Diagnostic

`CRATONVM_DBG_SWCHAIN=1` dumps, per chain entry, the recorded `entry_sp` /
`exact_rbp` / published compile id, the boundary method, how the innermost frame
resolved, and the walked activation list run-length-encoded. That dump is what
identified the defect, and the byte-dump technique that finished the job is
worth repeating: when `decode=<no-decode>` appears on a frame the mirror named,
print the twenty bytes before the return address and read the instruction — the
answer was in them.

## Related

- `retired/jit-self-recursive-activations-invisible-to-stack-walks-RETIRED-20260819.md`
  — the page this was found under, and the three defects closed before this one.
- `jit/src/x64/emit.rs` — `emit_call_absolute` / `emit_call_imm64_via_rax`, the
  two encodings.
- `vm/src/jit/conservative_roots.rs` — `direct_call_target`,
  `direct_call_target_abs64`, `innermost_frame_method`,
  `active_compiled_frames`.
- `vm/src/runtime/stackwalker.rs` — `capture_full_trace`,
  `interleave_compiled_frames`, `drop_osr_continuations`.

---

Everything below is the page as it stood when it was opened on `dev`, including
the redesign it proposed. Kept because its refuted hypotheses are what narrowed
the search, and because the shape it described is worth contrasting with what
was actually measured.

---

# A compiled frame BETWEEN two JIT chain entries is invisible to every Java stack walk

**Status: OPEN, isolated 2026-08-19 on `dev`. This is the last of four
mechanisms behind one symptom, and the only one still unfixed. It is what keeps
`stackwalker_log4j_deep_repeated_walks_finish_under_jit` red.**

## The failure

Log4j2's caller lookup answers with the enclosing class:

```
AssertionError: expected cratonvm.StackWalkerLog4jStressProbe$LoggerFactory
                but got cratonvm.StackWalkerLog4jStressProbe
```

`probes/SWStress3.java` instruments the failing walk itself — same call path,
same heat — and shows what the walk saw:

```
got=cratonvm.SWStress3   saw=Locator.getCallerClass | recurse | recurse | recurse | …
```

`LoggerFactory.resolveCaller` belongs between `getCallerClass` and `recurse` on
the real stack. It is absent, so the `dropWhile` chain runs past it and returns
the outer class.

## Mechanism

`resolveCaller` is a fully compiled method with a real frame. It has no
`JIT_ENTRY_CHAIN` entry because compiled code called it **directly**, and the
per-entry RBP walk never reaches it either:

* `getCallerClass` owns the innermost chain entry. Its `[rbp+8]` is a non-JIT
  return address (it was entered from the VM), so the walk that starts at that
  entry's `exact_rbp` stops immediately — `nested=1`.
* `resolveCaller` sits one frame further out, entered by a direct
  compiled→compiled call from `recurse`, so it pushes no entry of its own.
* `recurse`'s own chain entries start at their own frames and walk OUTWARD, so
  none of them descends to `resolveCaller` either.

It falls in the gap between two chain entries, and `active_compiled_frames`
walks a band per entry rather than the stack as a whole.

## What it is NOT — three refuted hypotheses, each measured

Recorded because each cost real time, and two of them look right from the code.

* **Not inlining.** `CRATONVM_DBG_JITC` says
  `inline-resolve REFUSED cratonvm/SWCross.helper()V depth=0:
  new/anewarray/multianewarray`, and the Log4j shape has **zero**
  `inline-splice` lines in the whole run while `resolveCaller` is
  `full-compile`d standalone and `bg-direct-call BOUND`. Also:
  `IrBuilder::build` does not inline at all — `Lowerer::inline_scopes` is empty
  on every compile, per its own doc comment.
* **Not the self-recursion collapse.** Fixed 2026-08-18 by the saved-RBP walk
  in `active_compiled_frames`; `SWShape` holds 67/67 and `SWMutual` 67 across
  tier-up.
* **Not the compile-id misnaming.** Fixed 2026-08-19: the single-pass backend
  never assigned `cm.compile_id`, so `bind_compile_id` bound 0 and every
  fast-tier innermost frame wore its caller's name. `SWFrames` went to
  HotSpot parity (n=68, `hasLoggerFactory=true`) — but the Log4j probe did not,
  which is what isolated the mechanism above.

## Where to start

One walk over the whole native stack instead of a band per chain entry: begin
at the innermost entry's `exact_rbp` and keep following saved-RBP links across
the non-JIT boundaries, using each chain entry's `entry_sp` only to attribute
`interp_depth`, not to bound the walk. The bounds and validation predicates in
`remap_active_jit_frames`' Stage 5 are the ones to copy; the read is
read-only, unlike that one.

Re-measure before assuming anything here. Every hypothesis in this file that
was not measured turned out to be wrong.

## Reproduction

```bash
javac -d /tmp/pb probes/SWStress3.java
CRATONVM_JIT_THRESHOLD=1 ./target/release/cratonvm --java-home <jdk25> \
  -cp /tmp/pb cratonvm.SWStress3
```

Expected on a fixed VM: `bad=0/32`, `STRESS3_OK`, matching HotSpot.

The suite test is

```bash
cargo test -p cratonvm-vm --release --test wp1_9_stackwalker \
  stackwalker_log4j_deep_repeated_walks_finish_under_jit
```

## Related

- retired/jit-self-recursive-activations-invisible-to-stack-walks-RETIRED-20260819.md
  — the page this was found under. Its own defect (self-recursive activations
  collapsing to one frame) is fixed; this is what remained.
- `vm/src/jit/conservative_roots.rs` — `JIT_ENTRY_CHAIN`,
  `active_compiled_frames`, `innermost_frame_method`.
- `vm/src/runtime/stackwalker.rs` — `capture_full_trace`,
  `interleave_compiled_frames`, `drop_osr_continuations`.
