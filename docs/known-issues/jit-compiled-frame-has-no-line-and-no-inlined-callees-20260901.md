# A JIT-compiled frame carried no line and its inlined callees no frames — FIXED 2026-09-01

| | |
|---|---|
| **Status** | **FIXED 2026-09-01** on `claude/audit-impl-20260901` @ `32f7d47c3`, branched from `dev` @ `56d6c3722`. All four defects closed. CratonVM's trace now matches HotSpot 25 **byte-for-byte on all three rows** of the witness. |
| **Symptom** | A stack trace captured after warm-up lost frames, printed `-1` for the line of a compiled frame, and reported the OSR back-edge instead of the real call site. Cold traces were correct. Nothing threw and nothing logged. |
| **Cause** | Four independent defects stacked on one row of the witness. The root of three of them is that `compiled_frame_entry` hard-coded `line_number: LINE_NUMBER_UNKNOWN` / `byte_code_index: -1` on a written-down premise — "no bci is recorded for a compiled frame" — that was **false**. |
| **Fix** | Recover the bci from the precise-oop-map PC→bci table; record a per-call inline-frame map beside it; publish a live-OSR-continuation registry and use it as a **display-only** bci override; and drop a compiled entry that is the same activation as an interpreter frame, on an opcode proof rather than a shape. |
| **Verified** | x86-64, one binary, one `.class` file, a **four-way kill-switch A/B** — see below. Each switch reverts exactly its own half. |
| **Not fixed** | The caller-attribution walk still cannot see an inlined method; no test pins any of this; optimizing-tier and aarch64 frames still carry no line; the NPE `because "…" is null` clause is still missing. See **What remains open**. |

**Severity, as filed:** high for diagnosability, zero for program results. Every
trace degraded silently and only *after* warm-up — that is, only in the runs
anyone cares about — and the degraded trace looked plausible.

## The witness

`probes/StackTraceAfterOsr.java` throws from the *same* site three times in one
process. Only the amount of prior warm-up differs. Compiled with `-g`; HotSpot
needs `-XX:-OmitStackTraceInFastThrow`, which otherwise drops the trace of a
repeated implicit NPE altogether (a different behaviour, not this one).

**Before (measured 2026-09-01, pre-fix):**

```
                   HotSpot 25 (-XX:-OmitStackTraceInFastThrow)  CratonVM (default)
before_any_warm    len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]  len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]
after_helper_warm  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]
after_main_osr     len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]  len=3 [leaf:25 probe:-1           main:62]
```

**After (same probe, branch built and run):**

```
                   HotSpot 25 (-XX:-OmitStackTraceInFastThrow)  CratonVM (branch)
before_any_warm    len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]  identical
after_helper_warm  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]  identical
after_main_osr     len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]  identical
```

The third row was the defect, and it was three defects stacked:

1. **`probe:-1`** — a compiled frame carried **no line number at all**.
2. **`mid` and `outer` gone** — they were inlined into `probe`'s artifact, and an
   inlined callee contributed no frame.
3. **`main:62` instead of `main:66`** — 62 is `main`'s OSR back-edge, 66 the
   actual call site.

`CRATONVM_JIT_NO_INLINE=1` isolated (1) from (2) and exposed a fourth, smaller
issue — the compiled frame was emitted *in addition to* its interpreter frame
rather than instead of it:

```
CRATONVM_JIT_NO_INLINE=1  →  hot=... leaf:9 leaf:-1 mid:-1 outer:-1 main:34
```

## The four-way A/B — this is the proof

Same binary, same `.class` file, `after_main_osr` row only. Every switch is
default-ON behaviour with an opt-out, registered in `types/tests/flag-surface.txt`,
`docs/config/flag-inventory.md` and `docs/flag-tokens.md`.

| arm | `after_main_osr` | what it isolates |
| --- | --- | --- |
| default | `len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]` | — |
| `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` | `len=3 [leaf:25 probe:42 main:66]` | (2) the inlined callees |
| `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` | `len=5 [leaf:25 mid:26 outer:27 probe:42 main:62]` | (3) the stale OSR back-edge line |
| `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` | `len=3 [leaf:25 probe:-1 main:62]` | (1), **and (2) and (3) with it** |
| `CRATONVM_DISABLE_JIT=1` | `len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]` | the interpreter reference |

### The three switches are a dependency chain, not three peers

```
CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1        reverts (1) — and takes (2) and (3) down with it
 ├── CRATONVM_JIT_NO_INLINE_FRAME_MAP=1       reverts (2) alone
 └── CRATONVM_JIT_NO_OSR_PC_REFRESH=1         reverts (3) alone
```

Read the arms downwards, not across. Arms 2 and 3 each revert exactly one half
and leave the other two standing, which is what makes them a clean two-arm
comparison. Arm 4 reproduces the **entire** pre-fix row — because both the
inline-frame lookup and the OSR override are keyed on the bci that (1) recovers,
so switching (1) off starves them. That is not a leak between switches; it is
the evidence that the three fixes share **one** recovered program point rather
than three independent recoveries, and it is why there is no arm that reverts
(2) or (3) *without* (1) available.

Defect (4) does not show on this row at all. It needed `CRATONVM_JIT_NO_INLINE=1`
to surface, and its own switch is `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE`.

An arm that changes nothing means that half never engaged, which is a different
finding from that half not working. Do not read the two as one.

## (1) The line number — a premise falsified by deopt

`compiled_frame_entry` hard-coded `LINE_NUMBER_UNKNOWN` / `-1` with a note that
no bci is recorded for a compiled frame. **The caution was right and the premise
was stale, and the thing that falsifies it was already in the tree:
deoptimisation reconstructs an interpreter frame from a compiled PC on every
single deopt.** A VM that can do that manifestly holds a PC→bci mapping. The
question was never whether one exists, only which one to read.

`conservative_roots::compiled_frame_bci(cm, rbp, native_pc)` reads the
**precise-oop-map** table (`OopMapEntry::native_pc_offset` → `bytecode_pc`)
rather than inventing a second mapping beside deopt's. Two keys, by position:

* for every frame **below the innermost**, the saved-RBP walk already read the
  return address into it, which is by construction the key
  `native_pc_offset` is recorded under — an exact hit names the bci of the call
  the frame is suspended in;
* for the **innermost** frame, which owns no return address on this stack, the
  safepoint-id slot at `[rbp - cm.sp_id_slot_off]`, accepted **only** when
  `find_oop_map_for_safepoint_id` confirms the artifact's own table recorded a
  map under that id.

`active_compiled_frames()` was split into a private
`active_compiled_frames_impl(want_bci)`; the historical four-tuple survives as a
thin `map` over it, so the GC root-scan diagnostic (`CRATONVM_DBG_JIT_ROOTSCAN`)
is untouched and pays nothing.

### It refuses in three cases, and that is the most important part

* **IR-backend artifacts** (`cm.used_ir_backend`). `ir_lower` stores a
  **monotonic safepoint counter starting at 1** in `OopMapEntry::bytecode_pc`,
  not a bci. Those counters are small integers, indistinguishable from plausible
  bcis, so reading one would resolve a real and confidently *wrong* line — the
  single worst outcome available here.
* **`cm.sp_id_slot_off == 0`** — the recorded flag for "compiled without the
  precise gate", and also true of *every* aarch64 artifact, whose backend
  hard-codes `bytecode_pc: 0` and would otherwise resolve every compiled frame
  to the first line of its method.
* **Any value ≥ 65536**, the JVMS 4.9.1 `code_length` bound. One spec-derived
  test, not a list of constants: it also rejects the single-pass backend's two
  synthetic pcs (`u32::MAX`, `u32::MAX - 1`), which this crate cannot name.

The RBP used for the sp-id read is bounds-checked against this thread's
`[scanner_sp, entry_sp)` band first: `innermost_frame_method` answers `Some(cm)`
even when the RBP is unusable, so a `[rbp - off]` read on an out-of-band RBP
would be a wild read rather than merely a wrong line.

## (2) The inlined callees — a map keyed on the same two things

*Producer*, `jit/src/x64/inlining.rs`: `InlineFrameMap`, built by
`begin_inline_frame_recording()` / `finish_inline_frame_recording(code_len)`
around a compile (opened in `x64/driver.rs`, retained on
`CompiledMethod::inline_frame_map`), recording one row per call emitted from
inside a spliced body. Rows are keyed on the *same two things* `compiled_frame_bci`
keys on — an exact `native_pc_offset` (the return address, recorded at
`self.buf.pos()` immediately after the `CALL`) and the safepoint id — and no
third key. Each row holds the chain of `(callee "class/Name.method:descriptor",
bci)` pairs, innermost-first.

It fails closed in three places: a level with an out-of-spec bci or an empty
label refuses the whole row; a rewound emission is dropped (explicit truncation
on every splice rollback path, plus a strictly-increasing-offset backstop in
`from_rows`); and a `safepoint_bci` two rows disagree about is **poisoned to
`None`** rather than resolved to either answer, because one bci covers a whole
spliced region and a splice containing two calls with different chains cannot be
told apart from the safepoint-id slot alone.

*Consumer*, `vm/src/jit/conservative_roots.rs`:
`active_compiled_frames_with_inline_chains()` and
`compiled_frame_inline_chain(cm_ptr, bci, native_pc)`. It calls
`active_compiled_frames_impl` directly rather than layering over the bci entry
point, because the walk already holds each non-innermost frame's return
address — the **exact** key — and the five-tuple threw it away. One rule is
worth naming: **an exact key that misses does not fall back to the coarse one.**
A miss on the return address means "this program point recorded no chain", not
permission to consult the safepoint id. `stackwalker::push_inlined_chain`
expands one compiled entry into one entry per level, reversing to outermost-first,
and stops at the first refusal rather than skipping it — losing a *suffix* of a
chain is recoverable by a reader; a chain with a hole is not.

The fail-closed rule here is stricter than the one for a line number, for a
stronger reason: a wrong line is visibly a line, while an inlined frame naming
the wrong method is indistinguishable from a real one.

### What was ruled out before writing the map, and how

* **`CompiledMethod::inlined_methods`** survives to runtime and names every
  spliced callee — but it is a flat *set* keyed by nothing. It exists for
  class-change invalidation, and cannot say which callee a given PC is inside,
  nor at which bci.
* **The deopt caller chain.** `DeoptimizationPoint::frame_state.caller` is a real
  chain, is retained, and is PC-indexed. Two facts killed it: every level carries
  `method_key: self.method_key` — the *compiling* method, because
  `build_frame_state_at` has no other identity to stamp — so a nested level would
  name the outer method with an inner method's bci; and the invoke arm inside a
  splice deliberately publishes no point at all (`emit_inline_invoke_into_rax`:
  "Deliberately NO `snapshot_pre_intrinsic_call` here"), so the one native offset
  a stack walk keys on has no deopt point under it. Making it publish one would
  record a resume bci the enclosing method does not have — the
  `IndexOutOfBoundsException`-into-`InternalError` regression of 2026-08-28.
* **`OopMapEntry`** is the right key and does survive, but has no spare field,
  and inside a splice its `bytecode_pc` holds the *enclosing* method's invoke
  bci. That is the answer `compiled_frame_bci` already returns, and the thing the
  chain has to *extend* rather than replace.

## (3) The stale OSR pc — a registry, and a display-only override

**`vm/src/runtime/interpreter/jit_bridge.rs`** publishes a live-OSR-continuation
registry: a thread-local `Vec<(interp_depth, cm_ptr)>`, pushed by an RAII
`OsrContinuationGuard` inside `try_osr` and withdrawn however control leaves —
normal return, OSR exit, routed exception, or a panic unwinding through
`catch_unwind`. `Drop` **truncates** to the pre-push length rather than popping
once, so a non-local exit out of a nested OSR entry cannot strand a descendant's
record. Cost is one push and one truncate per **OSR entry**, zero per back-edge
and zero in the compiled loop.

The mechanism, read off `jit_bridge.rs` rather than assumed: `try_osr` enters
through `osr_enter_planned` and the artifact runs the method **to its RETURN** —
compiled code does not hand control back at the loop exit. The interpreter
`Frame` for that activation therefore sits on `thread.frames` at `pc == entry_pc`
for the whole window, and any capture from a callee reports the loop header for a
method executing far below it. The *after* sub-case (the interpreter continuing
past the loop with a stale pc) was **checked and does not exist**: every exit
that leaves the frame alive already writes a pc
(`deopt_resume::transfer_osr_exit_into_live_frame` assigns `resume_bci`, the
RBC.6b handler entry assigns `handler_pc`), and the only other way out pops the
frame.

### Why `Frame::pc` was deliberately NOT advanced — the key decision on this page

`vm/src/runtime/stackwalker.rs` consumes the registry as a **display-only bci
override**: `(frame_index, bci, chain)` triples handed from
`drop_osr_continuations` to the entry builder, **never written back into
`Frame::pc`**. Advancing the pc would have been one line, would have fixed the
witness, and would have been a serious bug. Two independent reasons, either
sufficient:

* `Frame::live_locals_mask_here` and `Frame::scan_local_objects_inner` compute
  the per-bci live-locals **root filter** from `[self.pc, self.last_instr_pc]`.
  Moving `pc` to where compiled code really is would make every slot that dies
  between the back-edge and that point **stop being a GC root** — on a frame
  whose locals are the pre-OSR copies the conservative half of the JIT root scan
  is leaning on. A display fix would have become a use-after-free.
* The OSR **safe-reject** exit is correct only *because* `frame.pc` is still
  `entry_pc`. Moving it would resume the interpreter at a bci this activation
  never reached.

So the override is a side channel that exists for the trace and for nothing
else. Only the **authoritative** arm produces one: when the registry has nothing
to say (kill switch set, or a contended borrow) the old `cm.can_osr_enter(frame.pc)`
heuristic still drops the duplicate entry but does **not** move the bci. Its drop
is an inference, and a line carried across on an inference is the
confidently-wrong answer this area refuses everywhere else. That is also what
keeps `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` a clean two-arm A/B — one switch, both
the decision and the line revert together — rather than a half-revert.

### The latent bug found on the way

`drop_osr_continuations` decided "is this an OSR continuation?" with
`cm.can_osr_enter(frame.pc)` — **a property of a pc, not of an activation**. An
interpreted frame parked on a back-edge while a *recursive* compiled activation
of the same method was live satisfies that test too, and that activation's
compiled entry was then dropped from the trace: a real frame lost, the one
direction this function is otherwise careful never to fail in. The registry makes
the decision authoritative (`*cm_ptr == live_cm`, a plain `==` on the same
`Arc::as_ptr` encoding both sides already use); the heuristic remains only as the
fallback for when the registry answers `None`. **`None` from it means "no
information", never "this frame is interpreted".**

This was never on the witness and no measurement would have found it.

## (4) The duplicate frame — an opcode, not a heuristic

A second rule in `drop_osr_continuations`, resting on a proof rather than a
shape: the interpreter transfers control to another Java method in exactly one
way, by executing an `invoke*` opcode (`dispatch_static` / `dispatch_virtual` /
`dispatch_special` are the only callers of `execute_jit_call` and
`execute_jit_call_decoded`, each reached from its opcode arm). While a callee
runs, the caller frame's `last_instr_pc` names the invoke it is suspended in. So
a frame whose `last_instr_pc` does **not** hold one of the five invoke opcodes
(`0xb6`–`0xba`, none of which can be `wide`) cannot be the caller of anything; if
it names the same class, method and descriptor as a compiled entry at its own
depth, the only remaining reading is that the compiled entry is that frame's body.

Fail-safes:

* a `last_instr_pc` outside the frame's own `code` counts as **"caller"**, so the
  compiled entry survives — an extra frame, never a lost one;
* at most **one** entry is dropped per `(depth, label)`, and the two rules share
  **one** ledger rather than one each. Mutual recursion through compiled code
  (`foo` → `bar` → `foo`) puts two `foo` activations at one depth; with a ledger
  per rule the OSR rule could take the first and the call rule the second, and
  the trace would lose a real frame — the failure this function exists to avoid,
  reintroduced by the fix for it. First-wins is the right tie-break because
  entries sharing a depth arrive outermost-first;
* when the registry names a *different* artifact as the frame's body, rule 2 is
  **skipped** rather than consulted: an OSR'd frame is parked on a back-edge, so
  it is not at an invoke either — rule 2's premise holds and its conclusion does
  not;
* rule 2's drops deliberately produce **no** bci override. That the surviving
  interpreter frame's pc is stale was established and measured for the OSR case;
  for an ordinary compiled activation it has not been.

## What was ruled out, and how

* **The dedupe was not the cause of (3).** `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1`
  changed nothing on this witness. That single arm is what eliminated the
  duplicate-frame machinery as the explanation for the stale line, before any
  code was written: `drop_osr_continuations` was written for the *duplicate*
  frame, not the stale one, and the measurement said so.
* **`CRATONVM_DISABLE_JIT=1` restores everything** — same binary, same `.class`
  file — so none of this was ever a class-file, `LineNumberTable` or resolution
  problem.
* **`CRATONVM_JIT_NO_INLINE=1` separates (1) from (2)**, and is what exposed (4)
  at all.

### One open prediction, settled by the run

The pre-fix page predicted `main:66` at high confidence and explicitly refused to
predict `probe:42`: `probe`'s route off the IR tier could not be settled without
`javap`, and an IR-compiled `probe` would have kept `probe:-1` no matter what
else worked. The measured row is `probe:42`, so `probe`'s artifact is single-pass
and the IR refusal never fired. The recorded residual risk — that a safepoint
inside a splice might carry the *callee's* bci and resolve `probe` to something
in the 25–27 range — did not materialise either.

## What remains open

Ranked: security-relevant first, then regression risk, then the remaining
populations of line-less frames, then an unrelated fidelity gap.

1. **`frame_class_ids_with_compiled` still cannot see an inlined method** — and
   was deliberately left that way. That walk feeds `resolve_caller_class_id` (the
   JEP 403 deep-reflection gate and the member-modifier check) and the
   `Class.forName` caller-loader lookup. It answers in `ClassId`, an inlined
   level carries only an internal name, and resolving one by name from a JIT
   label to answer a *security* question would be a guess. It shares
   `active_compiled_frames_with_bci` and the same kept-set computation as
   `capture_full_trace`, so the two cannot disagree about which frames exist —
   only about how many entries one compiled frame expands into. **Behaviour is
   unchanged, but this is now a known blind spot in a security-relevant path**,
   and it is the top residual precisely because the display path no longer has it.
2. **No test pins any of the four fixes.** The commit touches no test file under
   `vm/` or `jit/`. `probes/StackTraceAfterOsr.java` is checked in, but nothing
   runs it automatically and nothing compares its three rows against the expected
   ones. Four behaviours, four kill switches, and the only thing standing between
   them and a silent regression is this page. An unpinned fix is one refactor
   from being undone.
3. **The optimizing (IR) tier still keeps `-1`.** `compiled_frame_bci` refuses
   `used_ir_backend` outright, for the good reason given above. This is the
   largest remaining population of line-less compiled frames, and nothing in a
   trace distinguishes it from the other two refusals. Closing it needs a
   safepoint-id→bci side table the artifact does not carry.
4. **x86-64 only.** aarch64 artifacts hard-code `bytecode_pc: 0` and are
   explicitly refused by `compiled_frame_bci` via `sp_id_slot_off == 0`, so a
   compiled frame there still reports no line. Nothing on this page has been run
   on aarch64.
5. **The NPE helpful message**, unrelated to the JIT and untouched. HotSpot:
   `Cannot load from int array because "StProbe.table[...]" is null`. CratonVM's
   interpreter: `Cannot load from int array` — the ` because "…" is null` clause
   is never built. The signal-derived path has a placeholder form
   (`because "<local>" is null`); the interpreter path has none. A pure
   `native-builtins` fidelity gap, to be closed separately.

## Why this mattered more than it looked

Every logging framework, every `catch (Exception e) { log.error("…", e); }`,
every Spring/Hibernate/Jackson diagnostic and every crash report reads
`getStackTrace()`. On a cold path they were correct. On a hot path — the one a
production incident is about — they named the wrong line, omitted the frames that
would have identified the caller, and dropped the NPE's explanatory clause. An
operator reading such a trace was not told that anything was missing.

## Reproducing it

`probes/StackTraceAfterOsr.java`, compiled with `-g` so the `LineNumberTable` is
present.

```sh
javac -g -d probes probes/StackTraceAfterOsr.java
java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceAfterOsr   # the oracle
cratonvm -cp probes StackTraceAfterOsr                              # all four ON
CRATONVM_JIT_NO_INLINE_FRAME_MAP=1     cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_OSR_PC_REFRESH=1       cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1 cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1 CRATONVM_JIT_NO_INLINE=1 cratonvm -cp probes StackTraceAfterOsr
CRATONVM_DISABLE_JIT=1 cratonvm -cp probes StackTraceAfterOsr       # the interpreter reference
```

Read the `after_main_osr` row and compare against the A/B table above.
