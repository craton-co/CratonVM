# A JIT-compiled frame now carries a line number; an inlined callee still contributes no frame

**Status:** PARTIALLY FIXED, **NOTHING RE-MEASURED**. Filed 2026-09-01 against
`dev` @ `56d6c3722`. Three of the four defects below have implementations in
the working tree of `claude/audit-impl-20260901` (uncommitted as of
2026-09-01, tree head `de4a07c4d`); the fourth is written on both sides and
wired on neither. **No agent on that branch could build or run**, so every
"fixed" on this page means *the code exists and reads correctly*, not *the
witness moved*. The witness table below is still the last measurement anyone
took, and it was taken **before** any of this landed.

**Severity:** high for diagnosability, zero for program results. Every trace
degrades silently and only *after* warm-up — that is, only in the runs anyone
cares about. Nothing throws, nothing logs, and the trace looks plausible.

## The one-run witness (measured 2026-09-01, pre-fix)

`probes/StackTraceAfterOsr.java` throws from the *same* site three times in one
process. Only the amount of prior warm-up differs.

```
                   HotSpot 25 (-XX:-OmitStackTraceInFastThrow)  CratonVM (default)
before_any_warm    len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]  len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]
after_helper_warm  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]
after_main_osr     len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]  len=3 [leaf:25 probe:-1           main:62]
```

The third row is the defect, and it is three defects stacked:

1. **`probe:-1`** — a compiled frame carries **no line number at all**.
2. **`mid` and `outer` are gone** — they were inlined into `probe`'s artifact,
   and an inlined callee contributes no frame.
3. **`main:62` instead of `main:66`** — 62 is `main`'s OSR back-edge, 66 is the
   actual call site. Once `main` OSR-enters, its interpreter `Frame.pc` stops
   advancing and every later trace reports the loop it tiered up in.

`CRATONVM_JIT_NO_INLINE=1` isolates (1) from (2) and exposed a fourth, smaller
issue — the compiled frame was emitted *in addition to* its interpreter frame
rather than instead of it:

```
CRATONVM_JIT_NO_INLINE=1  →  hot=... leaf:9 leaf:-1 mid:-1 outer:-1 main:34
```

`CRATONVM_DISABLE_JIT=1` restores all five frames, the correct lines and the
NPE message, on the same binary and the same `.class` file. That is the A/B,
and it is the only arm on this page that has actually been run since the fixes
were written — it does not exercise them at all.

## Status of the four defects

| # | defect | status | kill switch (default ON) |
| --- | --- | --- | --- |
| 1 | compiled frame has no line | implemented, unverified | `CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1` |
| 2 | inlined callee contributes no frame | **OPEN** — both halves written, neither reachable | `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` (inert today) |
| 3 | OSR frame reports its back-edge line | implemented, unverified | `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` |
| 4 | compiled frame emitted beside its interpreter frame | implemented, unverified | `CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1` |

All four names are registered in `types/tests/flag-surface.txt`,
`docs/config/flag-inventory.md` and `docs/flag-tokens.md`. The whole change is
therefore A/B-able inside ONE binary, which is the point: a line that looks
wrong in a warmed-up trace must be attributable to an environment variable
rather than to a rebuild.

**No test was added for any of them.** The branch diff touches no test file in
`vm/` or `jit/`, so the first evidence any of this works will be the probe.

## (1) The line number — where the bci comes from

`compiled_frame_entry` used to hard-code `line_number: LINE_NUMBER_UNKNOWN` /
`byte_code_index: -1`, with the note that no bci is recorded for a compiled
frame. **The caution was right and the premise was stale.** The precise-oop-map
machinery already emits a PC→bci table, and
`conservative_roots::compiled_frame_bci(cm, rbp, native_pc)` reads that table
rather than inventing a second mapping beside deopt's:

* for every frame **below the innermost**, the saved-RBP walk already read the
  return address into it, which is by construction the key
  `OopMapEntry::native_pc_offset` is recorded under — an exact hit names the
  bci of the call the frame is suspended in;
* for the **innermost** frame, which owns no return address on this stack, the
  safepoint-id slot at `[rbp - cm.sp_id_slot_off]`, accepted **only** when
  `find_oop_map_for_safepoint_id` confirms the artifact's own table recorded a
  map under that id.

`active_compiled_frames()` was split into a private
`active_compiled_frames_impl(want_bci)` returning
`(interp_depth, label, owner_class_id, cm_ptr, Option<u32> bci)`; the historical
four-tuple survives as a thin `map` over it, so the one out-of-scope consumer
(`vm/src/memory/roots.rs`'s `CRATONVM_DBG_JIT_ROOTSCAN` line) is untouched and
pays nothing. `active_compiled_frames_with_bci()` is the new entry point and is
what `capture_full_trace` and `frame_class_ids_with_compiled` now call.

### It refuses in three cases, and that is the most important thing here

* **IR-backend artifacts** (`cm.used_ir_backend`). `ir_lower` stores a
  **monotonic safepoint counter starting at 1** in `OopMapEntry::bytecode_pc`,
  not a bci. Those counters are small integers, indistinguishable from
  plausible bcis, so reading one would resolve a real and confidently *wrong*
  line — the single worst outcome available here. An IR-tier frame therefore
  keeps `-1` exactly as before. Closing this needs a safepoint-id→bci side
  table the artifact does not carry.
* **`cm.sp_id_slot_off == 0`** — the recorded flag for "compiled without the
  precise gate" (`precise_maps = precise_jit_maps_enabled() ||
  moving_young_enabled()`, in `jit/src/x64.rs`), and also true of *every*
  aarch64 artifact, whose backend hard-codes `bytecode_pc: 0` and would
  otherwise resolve every compiled frame to the first line of its method.
* **Any value ≥ 65536**, the JVMS 4.9.1 `code_length` bound. One spec-derived
  test, not a list of constants: it also rejects the single-pass backend's two
  synthetic pcs (`ENTRY_POLL_BC_PC` = `u32::MAX`, `SP_ID_UNSET_BC_PC` =
  `u32::MAX - 1`), which this crate cannot name.

The RBP used for the sp-id read is bounds-checked against this thread's
`[scanner_sp, entry_sp)` band before the read — `innermost_frame_method`
answers `Some(cm)` even when the RBP is unusable (it falls back to the boundary
method), so the caller cannot tell the two answers apart from the outside, and
`[rbp - off]` on an out-of-band RBP would be a wild read rather than merely a
wrong line.

## (2) The inlined callees — STILL OPEN

Both halves are written. **Neither is reachable, so behaviour is unchanged.**

*Producer*, `jit/src/x64/inlining.rs`: `InlineFrameMap`, built by
`begin_inline_frame_recording()` / `finish_inline_frame_recording(code_len)`
around a compile, recording one row per call emitted from inside a spliced
body. Rows are keyed on the *same two things* `compiled_frame_bci` keys on — an
exact `native_pc_offset` (the return address, recorded at `self.buf.pos()`
immediately after the `CALL` and before the republish/oop-map bytes move the
cursor) and the safepoint id (`cur_bc_pc`) — and no third key. Each row holds
the chain of `(callee "class/Name.method:descriptor", bci)` pairs,
innermost-first. It fails closed in three places: a level with an out-of-spec
bci or an empty label refuses the whole row; a rewound emission is dropped
(both by explicit truncation on every splice rollback path — five call sites,
each beside the `deopt_points` truncation it mirrors — and by a
strictly-increasing-offset backstop in `from_rows`); and a `safepoint_bci` two
rows disagree about is **poisoned to `None`** rather than resolved to either
answer, because one bci covers a whole spliced region and a splice containing
two calls with different chains cannot be told apart from the safepoint-id slot
alone.

*Consumer*, `vm/src/jit/conservative_roots.rs`:
`active_compiled_frames_with_inline_chains()` and
`compiled_frame_inline_chain(cm_ptr, bci)` — the latter **returns
`Vec::new()` unconditionally**, and nothing calls the former.

Three mechanical edits, in files owned by other agents on 2026-09-01, block it
(`.agent-requests/A18-jit-lib.txt` spells them out):

1. `jit/src/x64.rs:219` declares `mod inlining;` **privately**, with no
   `pub use` beside the ones its siblings have — so `InlineFrameMap` is not
   nameable outside `cratonvm_jit::x64`;
2. `CompiledMethod` (`jit/src/lib.rs`) has no field to retain the map on;
3. nothing opens a recording session — `begin_inline_frame_recording` and
   `finish_inline_frame_recording` have **no callers**, and every type in the
   producer carries `#[allow(dead_code)]`.

Until those land, `after_main_osr` keeps three frames where HotSpot has five,
and `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` switches off something that is already
off.

### What was ruled out before writing the map, and how

* **`CompiledMethod::inlined_methods`** survives to runtime and names every
  spliced callee — but it is a flat *set* keyed by nothing. It exists for
  class-change invalidation, cannot say which callee a given PC is inside, and
  cannot say at which bci.
* **The deopt caller chain.** `DeoptimizationPoint::frame_state.caller` is a
  real chain, is retained, and is PC-indexed. Two facts kill it: every level
  carries `method_key: self.method_key` — the *compiling* method — because
  `build_frame_state_at` has no other identity to stamp, so a nested level
  would name the outer method with an inner method's bci; and the invoke arm
  inside a splice deliberately publishes no point at all
  (`emit_inline_invoke_into_rax`: "Deliberately NO `snapshot_pre_intrinsic_call`
  here"), so the one native offset a stack walk keys on has no deopt point
  under it. Making it publish one would record a resume bci the enclosing
  method does not have — the `IndexOutOfBoundsException`-into-`InternalError`
  regression of 2026-08-28.
* **`OopMapEntry`** is the right key and does survive, but has no spare field,
  and inside a splice its `bytecode_pc` holds the *enclosing* method's invoke
  bci (`cur_bc_pc` is not moved by the inline walk). That is the answer
  `compiled_frame_bci` already returns, and the thing the chain has to *extend*
  rather than replace.

## (3) The stale OSR pc — a registry, and a display-only override

Two halves.

**`vm/src/runtime/interpreter/jit_bridge.rs`** publishes a live-OSR-continuation
registry: a thread-local `Vec<(interp_depth, cm_ptr)>`, pushed by an
RAII `OsrContinuationGuard` inside `try_osr` and withdrawn however control
leaves — normal return, OSR exit, routed exception, or a panic unwinding
through `catch_unwind`. `Drop` **truncates** to the pre-push length rather than
popping once, so a non-local exit out of a nested OSR entry cannot strand a
descendant's record. Cost is one push and one truncate per **OSR entry**, zero
per back-edge and zero in the compiled loop.

The mechanism, read off `jit_bridge.rs` rather than assumed: `try_osr` enters
through `osr_enter_planned` and the artifact runs the method **to its RETURN** —
compiled code does not hand control back at the loop exit. The interpreter
`Frame` for that activation therefore sits on `thread.frames` at `pc ==
entry_pc` for the whole window, and any capture from a callee reports the loop
header for a method executing far below it. The *after* sub-case (the
interpreter continuing past the loop with a stale pc) was **checked and does
not exist**: every exit that leaves the frame alive already writes a pc
(`deopt_resume::transfer_osr_exit_into_live_frame` assigns `resume_bci`, the
RBC.6b handler entry assigns `handler_pc`), and the only other way out pops the
frame.

**`vm/src/runtime/stackwalker.rs`** consumes it as a **display-only bci
override** — `(frame_index, bci)` pairs handed from `drop_osr_continuations` to
`entry_from_frame_at_bci`, never written back into `Frame::pc`. That restraint
is load-bearing, for two independent reasons either of which is sufficient:

* `Frame::live_locals_mask_here` and `Frame::scan_local_objects_inner` compute
  the per-bci live-locals **root filter** from `[self.pc, self.last_instr_pc]`.
  Advancing `pc` to where compiled code really is would make every slot that
  dies in between stop being a GC root — on a frame whose locals are the
  pre-OSR copies the conservative half of the JIT root scan is leaning on.
* The OSR **safe-reject** exit is correct only *because* `frame.pc` is still
  `entry_pc`. Moving it would resume the interpreter at a bci this activation
  never reached.

Only the **authoritative** arm produces an override. When the registry has
nothing to say (kill switch set, or a contended borrow) the old
`cm.can_osr_enter(frame.pc)` heuristic still drops the duplicate entry but does
**not** move the bci: its drop is an inference, and a line carried across on an
inference is the confidently-wrong answer this file refuses everywhere else.
That is what keeps `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` a clean two-arm A/B —
with it set, both the frame count and the line revert to the pre-2026-09-01
answer — rather than a half-revert.

### The latent bug found on the way

`drop_osr_continuations` decided "is this an OSR continuation?" with
`cm.can_osr_enter(frame.pc)` — **a property of a pc, not of an activation**. An
interpreted frame parked on a back-edge while a *recursive* compiled activation
of the same method was live satisfies that test too, and that activation's
compiled entry was then dropped from the trace: a real frame lost, the one
direction this function is otherwise careful never to fail in. The registry
makes the decision authoritative (`*cm_ptr == live_cm`, a plain `==` on the
same `Arc::as_ptr` encoding both sides already use); the heuristic remains only
as the fallback for when the registry answers `None`. `None` from it means
**"no information"**, never "this frame is interpreted".

## (4) The duplicate frame — an opcode, not a heuristic

A second rule in `drop_osr_continuations`, resting on a proof rather than a
shape: the interpreter transfers control to another Java method in exactly one
way, by executing an `invoke*` opcode (`dispatch_static` / `dispatch_virtual` /
`dispatch_special` are the only callers of `execute_jit_call` and
`execute_jit_call_decoded`, each reached from its opcode arm). While a callee
runs, the caller frame's `last_instr_pc` names the invoke it is suspended in.
So a frame whose `last_instr_pc` does **not** hold one of the five invoke
opcodes (`0xb6`–`0xba`, none of which can be `wide`) cannot be the caller of
anything; if it names the same class, method and descriptor as a compiled entry
at its own depth, the only remaining reading is that the compiled entry is that
frame's body.

Fail-safes:

* a `last_instr_pc` outside the frame's own `code` counts as **"caller"**, so
  the compiled entry survives — an extra frame, never a lost one;
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
  not, and letting it run would drop the genuine nested activation rule 1 had
  just declined to drop;
* rule 2's drops deliberately produce **no** bci override. That the surviving
  interpreter frame's pc is stale was established and measured for the OSR case;
  for an ordinary compiled activation it has not been.

## The prediction — UNVERIFIED, recorded as a prediction

Nobody could build. The agent that wired the stack walker predicted, for the
witness's `after_main_osr` row:

| frame | predicted | argument |
| --- | --- | --- |
| `main:66` | gains the real line | `compile_osr_artifact` reaches `x64::compile_with_param_slots` **directly** and never the IR pipeline, so the OSR artifact keeps `used_ir_backend: false` and the IR refusal cannot fire; `sp_id_slot_off != 0` because moving-young is default-on; and `main` is a *parent* in the saved-RBP walk, so its bci comes from the stronger `native_pc_offset` exact hit rather than the sp-id slot |
| `probe:42` | gains a line **if** its artifact is single-pass | stated at medium-high confidence — see the caveat below. **If `probe` is IR-compiled it stays `probe:-1`** |
| `leaf:25` | unchanged | interpreter frame, no compiled entry at its depth |
| `mid` / `outer` | absent | defect (2) is not wired |

So the predicted row is `len=3 [leaf:25 probe:42 main:66]` against HotSpot's
`len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]`.

**The `probe` argument as stated does not survive reading the source, and the
confidence should be read lower than medium-high.** The argument given was that
`probe` "returns a reference, carries an exception table, and its `catch`
handler reads a non-parameter local, which trips `precise_exception_frames`"
(the one term that keeps a method off the IR tier once
`CRATONVM_JIT_NO_EXC_TABLE_C2` stopped excluding exception tables wholesale).
But `regalloc::handler_has_unsafe_local_read` is a definitely-assigned dataflow
seeded **at the handler's own entry pc**, and a handler's first instruction is
the `astore` of its exception object — so a `catch` that reads only `e`, and
locals it assigned itself, marks those slots safe and does *not* trip RBC.6.
`probe`'s other route off the IR tier, `ir_unresumable_protected_trap`, needs a
deopt-guarded opcode (array access, `getfield`/`putfield`, integer division,
`arraylength`) **inside a protected range** of `probe`'s own bytecode, and
whether javac's `finally` copies put one there could not be settled without
`javap`. `probes/StackTraceAfterOsr.class` does not exist on the branch.
Treat `probe:42` as genuinely open, not as medium-high.

`main:66` is the sounder half — but note that if `main`'s OSR artifact is the
*innermost* compiled frame at capture time (nothing compiled runs below it,
because `probe` is re-entered through the interpreter), its bci comes from the
sp-id slot rather than the `native_pc_offset` hit. Both routes name the same
program point: the invoke at line 66 is a GC-capable call, so the emitter
stored that bci into the slot immediately before it.

**Residual risk, worth recording:** if the safepoint at the throw carries a
*spliced callee's* bci rather than `probe`'s own site bci, the line resolves
against `probe`'s `LineNumberTable` and prints something in the 25–27 range
instead of 42. That would be a **splice-bci defect, not a stackwalker one** —
`bytecode_pc` inside a splice is supposed to stay the enclosing method's invoke
bci.

### The command that settles it

```sh
# build the branch first (nobody has), then, from the repo root:
javac -g -d probes probes/StackTraceAfterOsr.java
java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceAfterOsr   # the oracle
cratonvm -cp probes StackTraceAfterOsr                              # all four ON
CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1 cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_OSR_PC_REFRESH=1       cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1 CRATONVM_JIT_NO_INLINE=1 cratonvm -cp probes StackTraceAfterOsr
CRATONVM_DISABLE_JIT=1 cratonvm -cp probes StackTraceAfterOsr       # the pre-existing A/B
```

Read the `after_main_osr` row. Arm 3 must reproduce `probe:-1`; arm 4 must
reproduce `main:62`; arm 5 must reproduce the doubled `leaf`. An arm that
changes nothing means that half never engaged, which is a different finding
from that half not working — do not read the two as one.

## What was ruled out, and how

* **The dedupe was not the cause of (3).** `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1`
  changed nothing on this witness. `drop_osr_continuations` was written for the
  *duplicate* frame, not the stale one, and the measurement said so before any
  code was written.
* **`CRATONVM_DISABLE_JIT=1` restores everything** — same binary, same
  `.class` file — so none of this is a class-file, `LineNumberTable` or
  resolution problem.
* **`CRATONVM_JIT_NO_INLINE=1` separates (1) from (2)** and is what exposed
  (4) at all.

## Still open, and unrelated to the JIT

The NPE **helpful message**. HotSpot says
`Cannot load from int array because "StProbe.table[...]" is null`; CratonVM's
interpreter says only `Cannot load from int array` — the ` because …` clause is
never built — and the compiled path says `null`. This is a pure
`native-builtins` fidelity gap, untouched by anything on this page, and should
be closed separately.

## Why this matters more than it looks

Every logging framework, every `catch (Exception e) { log.error("…", e); }`,
every Spring/Hibernate/Jackson diagnostic and every crash report reads
`getStackTrace()`. On a cold path they are correct. On a hot path — the one a
production incident is about — they name the wrong line, omit the frames that
would have identified the caller, and drop the NPE's explanatory clause. An
operator reading such a trace is not told that anything is missing.

Two things inside the VM read the same walk: `resolve_caller_class_id` (the
JEP 403 deep-reflection gate) and the `Class.forName` caller-loader lookup,
both of which already had to grow `frame_class_ids_with_compiled` for the
*frame* half of this problem. That function now shares
`active_compiled_frames_with_bci` and the same kept-set computation as
`capture_full_trace`, so the two cannot disagree about what "the frames of this
thread" are; it discards the display-only overrides, since it reports no lines.

## What would close it

1. **Run the probe.** Nothing on this page above the witness table has been
   measured. Everything else is downstream of that.
2. The three edits in `.agent-requests/A18-jit-lib.txt` (re-export
   `jit/src/x64.rs`'s `mod inlining;`, add the map field to `CompiledMethod`,
   open and close a recording session in `try_compile_inner`), then replace
   `compiled_frame_inline_chain`'s `Vec::new()` with the two-key lookup and
   point `capture_full_trace` at `active_compiled_frames_with_inline_chains`.
   Closes (2) — the only one of the four that is real work, and the only one
   still visible in the witness.
3. A safepoint-id→bci side table on IR artifacts, so an optimizing-tier frame
   stops keeping `-1`. This is the largest remaining population of line-less
   compiled frames, and today nothing distinguishes it from the other two
   refusals in a trace.
4. The `because "…" is null` clause for the interpreter's NPE messages.

## Reproducer

`probes/StackTraceAfterOsr.java`, compiled with `-g` so the `LineNumberTable`
is present. Run against HotSpot with `-XX:-OmitStackTraceInFastThrow` (HotSpot
otherwise drops the trace of a repeated implicit NPE altogether, which is a
different behaviour, not this one). The arms are listed under "The command that
settles it" above.
