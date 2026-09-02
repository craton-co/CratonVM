# A warmed-up stack trace is wrong: missing lines, missing frames, and a whole frame set lost when compiled code raises

| | |
|---|---|
| **Status** | **Defects 1–4 FIXED**, defect 5 **PARTIAL**. Opened 2026-09-01 on `dev` @ `56d6c3722`; re-measured and re-scoped the same day on `dev` @ `f4fc5f66f`. (1) and (3) closed on `dev` by `b2543ae13`, (5) partially by `70c49bc2d`; (2) and (4) closed independently on `claude/audit-impl-20260901` and merged into that work. |
| **Symptom** | A stack trace captured after warm-up lost frames, printed `-1` for the line of a compiled frame, and reported the OSR back-edge instead of the real call site. Cold traces were correct. Nothing threw and nothing logged. |
| **Cause** | Five independent defects, four of them stacked on one row of one witness. The root of three is that `compiled_frame_entry` hard-coded `line_number: LINE_NUMBER_UNKNOWN` / `byte_code_index: -1` on a written-down premise — "no bci is recorded for a compiled frame" — that was **false**. |
| **Fix** | Carry the bci the walk was already computing and throwing away; record a per-call inline-frame map beside the oop maps; publish a live-OSR-continuation registry and use it as a **display-only** bci override; drop a compiled entry that is the same activation as an interpreter frame, on an opcode proof rather than a shape; and snapshot the compiled frames at an implicit NPE before they unwind. |
| **Verified** | Each half was measured on its own binary, x86-64, one `.class` file, with a **four-way kill-switch A/B**. **The merged binary has not been re-run** — see "What the merge changed, and what still has to be measured". |
| **Not fixed** | (5) is partial. The caller-attribution walk still cannot see an inlined method; optimizing-tier and aarch64 frames still carry no line; the NPE `because "…" is null` clause is still missing. See **What remains open**. |

**Severity, as filed:** high for diagnosability, zero for program results. Every
trace degraded silently and only *after* warm-up — that is, only in the runs
anyone cares about — and the degraded trace looked plausible.

## Scoreboard

| # | Defect | State |
|---|---|---|
| 1 | A compiled frame carries no line number | **FIXED** — `b2543ae13` (dev) |
| 3 | An OSR-entered frame reports its back-edge, not its call site | **FIXED** — `b2543ae13` (dev), with the continuation registry from the audit branch |
| 2 | An inlined callee contributes no frame | **FIXED** — audit branch |
| 4 | A compiled activation emitted beside its interpreter frame | **FIXED** — audit branch — but see the warning below: the lever the original page used to see it **does not exist** |
| 5 | An exception raised BY compiled code loses every compiled frame | **PARTIAL** — `70c49bc2d`; not on the original page at all |

The numbering is the original page's, kept so its cross-references still land.
Ordered by user-visible damage it is 5 > 2 > 1 ≈ 3, which is *not* the order the
original page proposed: it called (2) "the only one that is real work" and did
not know about (5).

## The one-run witness

`probes/StackTraceAfterOsr.java` throws from the *same* site three times in one
process. Only the amount of prior warm-up differs. Compiled with `-g`; HotSpot
needs `-XX:-OmitStackTraceInFastThrow`, which otherwise drops the trace of a
repeated implicit NPE altogether (a different behaviour, not this one).

```
                   HotSpot 25                                       CratonVM before                  dev, (1)+(3)                     audit branch, (1)-(4)
before_any_warm    len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]  same                             same                             same
after_helper_warm  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]  same                             same                             same
after_main_osr     len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]  len=3 [leaf:25 probe:-1 main:62] len=3 [leaf:25 probe:42 main:66] len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]
```

`CRATONVM_DISABLE_JIT=1` restores all five frames on the same binary and the
same `.class` file. That is the A/B, and it is why none of this was ever a
class-file, `LineNumberTable` or resolution problem.

The third row was four defects stacked:

1. **`probe:-1`** — a compiled frame carried **no line number at all**.
2. **`mid` and `outer` gone** — they were inlined into `probe`'s artifact, and an
   inlined callee contributed no frame.
3. **`main:62` instead of `main:66`** — 62 is `main`'s OSR back-edge, 66 the
   actual call site.
4. and, off this row, a compiled activation emitted *in addition to* its
   interpreter frame rather than instead of it.

## What the merge changed, and what still has to be measured

The two halves were developed in parallel and each was measured on **its own
binary**. The merge kept `dev`'s carrier and re-expressed the audit branch's
halves on top of it:

* the `(depth, label, class_id, cm_ptr)` tuple is `ActiveCompiledFrame`, `dev`'s
  struct, extended with one field — `inline_chain` — for (2);
* the bci comes from `dev`'s `activation_bci` (one slot read plus the artifact's
  own confirmation), **not** from the audit branch's two-source
  `compiled_frame_bci`. The audit branch's two extra refusals were carried over
  into it, because they fail closed: an `used_ir_backend` artifact and a bci
  outside the JVMS 4.9.1 range are both rejected;
* the audit branch's `bci_lookup_census` (`[JIT_BCI_LOOKUPS]`, eight counters
  separating an exact hit from a silent fall-back to the coarse key) went with
  the two-source lookup it described. There is now one evidence source, so the
  distinction it existed to expose no longer exists;
* the inline chain keeps **both** keys, because it needs them: the exact return
  address for a frame below the innermost, the safepoint id for the innermost,
  and no fall-back between them.

**Nothing in this section has been re-run.** The predicted `after_main_osr` row
for the merged binary is `len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]` —
HotSpot-exact — and the four A/B arms below are predicted to hold unchanged.
Treat both as claims awaiting a run, not as measurements.

## The four-way A/B — this is the proof

Same binary, same `.class` file, `after_main_osr` row only. Every switch is
default-ON behaviour with an opt-out, registered in all four of the places
`flag_groups::INVENTORY`'s doc names.

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
comparison. Arm 4 reproduces the **entire** pre-fix row — because `probe` is the
innermost activation of its chain entry, so it owns no return address on this
stack and both the inline-frame lookup and the OSR override are reached through
the bci that (1) recovers. Switching (1) off starves them. That is not a leak
between switches; it is the evidence that the three fixes share **one** recovered
program point rather than three independent recoveries, and it is why there is
no arm that reverts (2) or (3) *without* (1) available.

Defect (4) does not show on this row at all, and its own switch is
`CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1`. `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1`
still turns off both dedupe rules together.
`CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON=1` reverts the emitter-side refusal
described under (2) alone, and `CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1` reverts
(5).

An arm that changes nothing means that half never engaged, which is a different
finding from that half not working. Do not read the two as one.

## (1) and (3) — FIXED

Both are one fact: **the bytecode index was already there, and the walk was
throwing it away.**

Every live compiled frame publishes the bytecode PC of its current safepoint
into `[rbp - cm.sp_id_slot_off]`; `conservative_roots::active_safepoint_id` has
read it for the GC walkers all along. `active_compiled_frames` computes the RBP
of every activation *in order to find the next one* and then dropped it,
returning a tuple with no bci — so `stackwalker::compiled_frame_entry`, the only
consumer that wants one, could do nothing but hard-code `LINE_NUMBER_UNKNOWN`.
Carrying that RBP four lines further is the whole fix. The tuple is now an
`ActiveCompiledFrame` carrying the bci.

The premise that had to be falsified first is worth recording, because it is the
reason this survived: the note said no bci is recorded for a compiled frame.
**The caution was right and the premise was stale, and the thing that falsifies
it was already in the tree — deoptimisation reconstructs an interpreter frame
from a compiled PC on every single deopt.** A VM that can do that manifestly
holds a PC→bci mapping. The question was never whether one exists, only which
one to read.

### The id is not trusted on sight

`activation_bci` requires the artifact to also name it as one of **its own**
recorded safepoints (`find_oop_map_for_safepoint_id`). The slot is written
before a call, so between calls it holds the *previous* safepoint; an artifact
that reserves no slot leaves uninitialised memory there. Both are rejected, and
an unusable id keeps the old answer. The original page's rule stands — a wrong
line is worse than none — but a right one is now available for the overwhelming
majority of frames.

Three further refusals, and they are the most important part:

* **IR-backend artifacts** (`cm.used_ir_backend`). `ir_lower` stores a
  **monotonic safepoint counter starting at 1** in `OopMapEntry::bytecode_pc`,
  not a bci. Those counters are small integers indistinguishable from plausible
  bcis, **and the artifact's own table records them**, so the confirmation above
  passes and a real, confidently *wrong* line comes out — the single worst
  outcome available here.
* **`cm.sp_id_slot_off == 0`** — the recorded flag for "compiled without the
  precise gate", and also true of *every* aarch64 artifact, whose backend
  hard-codes `bytecode_pc: 0` and would otherwise resolve every compiled frame
  to the first line of its method. `active_safepoint_id` already refuses this.
* **Any value ≥ 65536**, the JVMS 4.9.1 `code_length` bound. One spec-derived
  test, not a list of constants: it also rejects the single-pass backend's two
  synthetic pcs (`u32::MAX`, `u32::MAX - 1`), which this crate cannot name.

The RBP used for the sp-id read is bounds-checked against this thread's
`[scanner_sp, entry_sp)` band first: `innermost_frame_method` answers `Some(cm)`
even when the RBP is unusable, so a `[rbp - off]` read on an out-of-band RBP
would be a wild read rather than merely a wrong line.

### (3) falls out of the same bci

`drop_osr_continuations` was deleting the one thing that knew where the
OSR-entered method actually was: the continuation itself. `try_osr` enters
through `osr_enter_planned` and the artifact runs the method **to its RETURN** —
compiled code does not hand control back at the loop exit. The interpreter
`Frame` for that activation therefore sits on `thread.frames` at
`pc == entry_pc` for the whole window, and any capture from a callee reports the
loop header for a method executing far below it. The dropped activation's bci is
now handed to the frame that survives.

The *after* sub-case — the interpreter continuing past the loop with a stale pc —
was **checked and does not exist**: every exit that leaves the frame alive
already writes a pc (`deopt_resume::transfer_osr_exit_into_live_frame` assigns
`resume_bci`, the RBC.6b handler entry assigns `handler_pc`), and the only other
way out pops the frame.

### Why `Frame::pc` was deliberately NOT advanced — the key decision on this page

Both sessions reached this independently, which is the strongest evidence on the
page that it is right. The override is applied **during capture** and is **never
written back into `Frame::pc`**. Advancing the pc would have been one line, would
have fixed the witness, and would have been a serious bug. Two independent
reasons, either sufficient:

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
else. No resume path and no root scan can observe it, because it is never
stored.

### The latent bug found on the way, and the registry that closes it

`drop_osr_continuations` decided "is this an OSR continuation?" with
`cm.can_osr_enter(frame.pc)` — **a property of a pc, not of an activation**. An
interpreted frame parked on a back-edge while a *recursive* compiled activation
of the same method was live satisfies that test too, and that activation's
compiled entry was then dropped from the trace: a real frame lost, the one
direction this function is otherwise careful never to fail in.

`vm/src/runtime/interpreter/jit_bridge.rs` publishes a live-OSR-continuation
registry: a thread-local `Vec<(interp_depth, cm_ptr)>`, pushed by an RAII
`OsrContinuationGuard` inside `try_osr` and withdrawn however control leaves —
normal return, OSR exit, routed exception, or a panic unwinding through
`catch_unwind`. `Drop` **truncates** to the pre-push length rather than popping
once, so a non-local exit out of a nested OSR entry cannot strand a descendant's
record. Cost is one push and one truncate per **OSR entry**, zero per back-edge
and zero in the compiled loop.

The decision is now authoritative (`f.cm_ptr == live_cm`, a plain `==` on the
same encoding both sides already use); the heuristic remains only as the
fallback for when the registry answers `None`. **`None` from it means "no
information", never "this frame is interpreted"** — it is equally what
`CRATONVM_JIT_NO_OSR_PC_REFRESH=1` and a contended borrow report.

Only the **authoritative** arm produces an override. The heuristic arm still
drops the duplicate entry but does **not** move the bci: its drop is an
inference, and a line carried across on an inference is the confidently-wrong
answer this area refuses everywhere else. That is also what keeps
`CRATONVM_JIT_NO_OSR_PC_REFRESH=1` a clean two-arm A/B — one switch, both the
decision and the line revert together — rather than a half-revert.

This latent bug was never on the witness and no measurement would have found it.

## (2) The inlined callees — a map keyed on the same two things

`active_compiled_frames()` was flat: one entry per compiled artifact. HotSpot's
equivalent is a `ScopeDesc` *chain* — an inlined callee is a nested scope on the
same PC, which is what makes `mid`/`outer` reappear.

*Producer*, `jit/src/x64/inlining.rs`: `InlineFrameMap`, built by
`begin_inline_frame_recording()` / `finish_inline_frame_recording(code_len)`
around a compile (opened in `x64/driver.rs`, retained on
`CompiledMethod::inline_frame_map`), recording one row per call emitted from
inside a spliced body. Rows are keyed on the *same two things* the bci recovery
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
`ActiveCompiledFrame::inline_chain`, filled by
`compiled_frame_inline_chain(cm, bci, native_pc)`. The walk reports each
activation's own return address alongside its RBP, because that is the exact key
and it already had it. One rule is worth naming: **an exact key that misses does
not fall back to the coarse one.** A miss on the return address means "this
program point recorded no chain", not permission to consult the safepoint id —
the calls emitted on the inline cache's **miss edge** sit under the same
`cur_bc_pc` as the splice beside them while recording no row of their own, so
the coarse key would hand a frame the chain of a method that never ran.

`stackwalker::push_inlined_chain` expands one compiled entry into one entry per
level, reversing to outermost-first, and stops at the **first refusal rather
than skipping it** — losing a *suffix* of a chain is recoverable by a reader
(the trace is visibly short); a chain with a hole is not. The fail-closed rule
here is stricter than the one for a line number, for a stronger reason: a wrong
line is visibly a line, while an inlined frame naming the wrong method is
indistinguishable from a real one.

An inlined level carries no `ClassId` — the emitter knew the callee only by its
internal name — so `stackwalker::inlined_frame_entry` resolves it through
`find_class_id_by_name_memoized`. The memo is not optional: `find_by_name` is an
O(n) scan of every loaded class, and this would run per level, per frame, on
every VM-raised throw. It follows the same three rules as the method-slot memo
beside it — never memoize a negative, verify on every hit, retain nothing.

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
  bci. `inlining.rs` **never assigns `cur_bc_pc`**, which is why `probe:42` is
  recoverable at all — and it is also why the chain had to be recorded
  *separately* rather than by moving `cur_bc_pc`, which is simultaneously the
  safepoint id, the deopt resume point and the GC map key.
* **`try_emit_nested_inline` snapshots and rolls back eleven pieces of emitter
  state.** The new table joins that list; a rolled-back splice that left phantom
  rows in the map would be worse than the missing frames, because a phantom
  frame reads as real.

## (4) The duplicate frame — an opcode proof, and a witness that never existed

**Read this warning before the fix.** The original page isolated (4) with:

```
CRATONVM_JIT_NO_INLINE=1  →  hot=... leaf:9 leaf:-1 mid:-1 outer:-1 main:34
```

**`CRATONVM_JIT_NO_INLINE` does not exist.** It is absent from
`types/tests/flag-surface.txt` and nothing in the tree reads it; the only
substring match is the unrelated `CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP`. That
arm ran the default configuration and isolated nothing. The quoted output's
method names and line numbers (`hot=`, `leaf:9`, `main:34`) match no committed
probe either, so it came from one that was never checked in. Setting the
variable on the shipped witness changes nothing, on the pre-fix and post-fix
binaries alike, and `probes/StackTraceCompiledCallee.java` — the real version of
that arm, done in Java by padding the callee past the inline size cap — shows no
duplicated frame.

**So the shipped fix rests on a proof, not on a measurement**, and that is how
it should be read. The interpreter transfers control to another Java method in
exactly one way, by executing an `invoke*` opcode (`dispatch_static` /
`dispatch_virtual` / `dispatch_special` are the only callers of
`execute_jit_call` and `execute_jit_call_decoded`, each reached from its opcode
arm). While a callee runs, the caller frame's `last_instr_pc` names the invoke it
is suspended in. So a frame whose `last_instr_pc` does **not** hold one of the
five invoke opcodes (`0xb6`–`0xba`, none of which can be `wide`) cannot be the
caller of anything; if it names the same class, method and descriptor as a
compiled entry at its own depth, the only remaining reading is that the compiled
entry is that frame's body. This is the same argument `can_osr_enter` makes,
stated over the opcode rather than over one artifact's OSR entry list.

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
  not, because the caller of that entry is the frame's own compiled body;
* rule 2's drops deliberately produce **no** bci override. That the surviving
  interpreter frame's pc is stale was established and measured for the OSR case;
  for an ordinary compiled activation it has not been.

`CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1` turns this half off on its own. Its revert
shape is **not** asserted by any test, deliberately: the only place it was ever
observed is behind a variable that does not exist, and a check whose expected
output nobody has measured is a false red waiting to happen.

## (5) An exception raised BY compiled code loses its frames — PARTIAL

Not on the original page, and the largest of the five.

An implicit NPE in compiled code is not thrown where it happens. The null check
is an inline `TEST/JZ` to a shared stub (`x64/arrays.rs`); the stub calls
`jit_npe_with_action`, loads the `i64::MIN` deopt sentinel and **runs the
epilogue**; the `java/lang/NullPointerException` is constructed afterwards, from
the interpreter. The VM's own comment on `take_jit_pending_npe` says so
outright. `fillInStackTrace` therefore runs on a stack the compiled frames have
already left.

Witness: `probes/StackTraceCompiledCallee.java`. Its callee is padded past
`MAX_INLINE_BYTECODE_SIZE` (325) so it is compiled and **called** rather than
spliced.

```
              HotSpot                      before                  after 70c49bc2d
cold          [big:42 probe:56 main:65]    same                    same
after_warm    [big:42 probe:56 main:67]    [probe:56 main:67]      [big:-1 probe:56 main:67]
after_osr     [big:42 probe:56 main:71]    [main:71]               [main:71]
```

The fix snapshots the live compiled frames inside the helper as
`Vec<ActiveCompiledFrame>` — the same small structs the GC root walk already
builds, no class-store lookup, no formatting, no lock — and renders them once,
later, only for a throwable that was actually constructed. The snapshot is
drained in `take_all_jit_signals` alongside every other out-of-band JIT signal.
**That placement is the fix, not tidiness**: the first cut stashed it outside
the one-shot drain and was completely inert, because this shape is constructed
at three `if sig.npe` sites in `jit_bridge.rs` that the two
`take_jit_pending_npe()` sites never reach. A signal that is not part of
`DrainedJitSignals` is a signal some path will drop.

Ordering is load-bearing and easy to get backwards: a **stored** trace is
outermost-first (the Java array is built by reversing it), so the snapshotted
frames — the innermost ones — append. The first attempt prepended, and printed
the throw site as the outermost frame: a plausible-looking trace of a
completely different call.

Since the merge, `append_snapshotted_compiled_frames` renders through the same
`push_compiled_frames` builder the live splice uses, so a frame recovered this
way also expands its inlined callees. That is new behaviour and is **untested**.

`CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1` restores the old answer and is verified
to do so — a switch that changed nothing would mean the snapshot was not what
was doing the work, which is exactly how the inert first cut looked.

**Still open in (5):**

* **The recovered frame has no line** (`big:-1`). The inline null check is not a
  GC-capable call, so it publishes no safepoint id, and `activation_bci`
  correctly refuses the stale one in the slot. The emitter *does* hold the
  trapping bci (`emit_null_check_array_load_at` takes `bc_pc`), but the only
  places to put it are the sp-id slot — which also selects the oop map for GC —
  or a per-site `MOV imm32` on every array null check. Neither is worth a line
  number; closing this needs a side channel that is not the GC's.
* **`after_osr` is unchanged.** `CRATONVM_DBG_STTRACE=1` emits no
  `STTRACE_DBG_NPE_SNAPSHOT` line for that arm at all and reports
  `frames=1 depth=1` at capture, so it is constructed through a drain site still
  unaccounted for. That is the next thing to find.
* **`big:42` appears in 2 runs of 5 and `big:-1` in 3.** Not flakiness in the
  fix — it is whether `big` was compiled at that instant, so the frame comes
  from the interpreter (real line) or from the snapshot (`-1`). Recorded because
  a single run of this arm read `[big:42 probe:56 main:67]`, which is
  HotSpot-identical, and reporting that from one run would have been a false
  claim.

## The regression tests, and what each covers

Two exist, from the two sessions, and they cover different things. Keep both.

| test | covers |
|---|---|
| `regression-suite/src/RJitStackTraceLines.java` | (1) and (3), against HotSpot 25. Asserts **no line constants**: the same site is reached cold and hot and the two traces must agree, so editing the file cannot make it vacuously true. Fails on the pre-fix binary (`osr: probe has no line number (-1)`) and is byte-identical to HotSpot on the fixed one. |
| `vm/tests/stack_trace_across_tiers.rs` | all four rows of the witness, plus the kill switches. Uses `CRATONVM_DISABLE_JIT=1` on the **same** binary and `.class` file as the oracle, so it can never go stale on a probe edit; asserts the frame count and method sequence, that no frame reports a non-positive line, and that `CRATONVM_JIT_NO_COMPILED_FRAME_LINES`, `CRATONVM_JIT_NO_INLINE_FRAME_MAP` and `CRATONVM_JIT_NO_OSR_PC_REFRESH` each still revert their own half. |

> `RJitStackTraceLines` throws **explicitly**. A first draft dereferenced null
> and failed on **HotSpot**, not on CratonVM: `OmitStackTraceInFastThrow` is on
> by default and replaces a repeated implicit exception's trace with a frameless
> shared one. A cross-VM oracle has to be reachable without a `-XX:` flag the
> suite does not pass.

`CRATONVM_JIT_NO_CALL_FRAME_DEDUPE` is deliberately not asserted — see (4).

## What was ruled out, and how

* **The dedupe was not the cause of (3).** `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1`
  changed nothing on this witness. That single arm is what eliminated the
  duplicate-frame machinery as the explanation for the stale line, before any
  code was written: `drop_osr_continuations` was written for the *duplicate*
  frame, not the stale one, and the measurement said so.
* **`CRATONVM_DISABLE_JIT=1` restores everything** — same binary, same `.class`
  file — so none of this was ever a class-file, `LineNumberTable` or resolution
  problem.
* **`CRATONVM_JIT_NO_INLINE=1` separates nothing.** It does not exist. See (4).

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
   `active_compiled_frames` and the same kept-set computation as
   `capture_full_trace`, so the two cannot disagree about which frames exist —
   only about how many entries one compiled frame expands into. **Behaviour is
   unchanged, but this is now a known blind spot in a security-relevant path**,
   and it is the top residual precisely because the display path no longer has
   it. The 93-line proof that no consumer of that walk can be standing on an
   inlined frame is carried in the function's own doc comment; it is
   load-bearing and re-deriving it costs a day.
2. **(5) is partial** — three named residuals above, the `after_osr` drain site
   being the next thing to find.
3. **The merged binary has not been run.** Every measurement on this page comes
   from one of the two pre-merge binaries. Re-run the witness and the four A/B
   arms before treating the top table as measured.
4. **The optimizing (IR) tier still keeps `-1`.** `activation_bci` refuses
   `used_ir_backend` outright, for the good reason given above. This is the
   largest remaining population of line-less compiled frames, and nothing in a
   trace distinguishes it from the other two refusals. Closing it needs a
   safepoint-id→bci side table the artifact does not carry.
5. **x86-64 only.** aarch64 artifacts hard-code `bytecode_pc: 0` and are
   explicitly refused via `sp_id_slot_off == 0`, so a compiled frame there still
   reports no line. Nothing on this page has been run on aarch64.
6. **The bci lookup no longer counts its own evidence.** The audit branch's
   `bci_lookup_census` separated an exact `native_pc_offset` hit from a silent
   fall-back to the safepoint-id slot — a distinction that mattered because the
   exact lookup had missed *every* time for as long as an emitter defect filed
   oop maps 9–25 bytes past the return address, while the fallback answered and
   nothing observable said which evidence produced it. The merged bci recovery
   has one evidence source, so that census went with it. The lesson survives the
   code: **a silent fallback that produces a plausible answer is
   indistinguishable, from the outside, from the precise path working.** The
   emitter-side half, `jit::x64::inline_call_map_at_return_counts()`, is still
   there and is still the way to see whether the inline map's exact key is being
   spent.
7. **The NPE helpful message**, unrelated to the JIT and untouched. HotSpot:
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
would have identified the caller, and, when compiled code was what raised,
omitted the raising method entirely. An operator reading such a trace was not
told that anything was missing.

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
CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1    cratonvm -cp probes StackTraceAfterOsr
CRATONVM_DISABLE_JIT=1 cratonvm -cp probes StackTraceAfterOsr       # the interpreter reference

# (5) — a compiled, non-inlined callee
java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceCompiledCallee
cratonvm -cp probes StackTraceCompiledCallee
CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1 cratonvm -cp probes StackTraceCompiledCallee
CRATONVM_DBG_STTRACE=1 cratonvm -cp probes StackTraceCompiledCallee   # snapshot census
```

Read the `after_main_osr` row and compare against the A/B table above. Run (5)'s
arms **several times**: which frames carry a line depends on what was compiled at
that instant, and one run is not a measurement.
