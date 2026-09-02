# A warmed-up stack trace is wrong: missing lines, missing frames, and a whole frame set lost when compiled code raises

**Status:** OPEN, partially fixed. Opened 2026-09-01 on `dev` @ `56d6c3722`;
re-measured and re-scoped the same day on `dev` @ `f4fc5f66f`, x86-64 **Windows**
(the original was Linux — every row below reproduces on both), default flags,
real-JDK mode, JDK 25.

**Severity:** high for diagnosability, zero for program results. Every trace
degrades silently and only *after* warm-up — that is, only in the runs anyone
cares about. Nothing throws, nothing logs, and the trace looks plausible.

## Scoreboard

| # | Defect | State |
|---|---|---|
| 1 | A compiled frame carries no line number | **FIXED** — `b2543ae13` |
| 3 | An OSR-entered frame reports its back-edge, not its call site | **FIXED** — `b2543ae13` |
| 5 | An exception raised BY compiled code loses every compiled frame | **PARTIAL** — `70c49bc2d`; not on the original page at all |
| 2 | An inlined callee contributes no frame | OPEN |
| 4 | A compiled activation emitted beside its interpreter frame | **UNREPRODUCED** — the lever the page used to see it does not exist |

The numbering is the original page's, kept so its cross-references still land.
Ordered by user-visible damage it is 5 > 2 > 1 ≈ 3, which is *not* the order the
original page proposed: it called (2) "the only one that is real work" and did
not know about (5).

## The one-run witness

`probes/StackTraceAfterOsr.java` throws from the *same* site three times in one
process. Only the amount of prior warm-up differs.

```
                   HotSpot 25 (-XX:-OmitStackTraceInFastThrow)      CratonVM before                   CratonVM now
before_any_warm    len=5 [leaf:25 mid:26 outer:27 probe:42 main:54] same                              same
after_helper_warm  len=5 [leaf:25 mid:26 outer:27 probe:42 main:58] same                              same
after_main_osr     len=5 [leaf:25 mid:26 outer:27 probe:42 main:66] len=3 [leaf:25 probe:-1 main:62]  len=3 [leaf:25 probe:42 main:66]
```

`CRATONVM_DISABLE_JIT=1` restores all five frames on the same binary and the
same `.class` file. That is the A/B. The two numbers that were wrong are now
HotSpot-exact and stable over repeated runs; the two frames still missing are
defect (2).

## (1) and (3) — FIXED

Both are one fact: **the bytecode index was already there, and the walk was
throwing it away.**

Every live compiled frame publishes the bytecode PC of its current safepoint
into `[rbp - cm.sp_id_slot_off]`; `conservative_roots::active_safepoint_id` has
read it for the GC walkers all along. `active_compiled_frames` computes the RBP
of every activation *in order to find the next one* and then dropped it,
returning a `(depth, label, class_id, cm_ptr)` tuple with no bci — so
`stackwalker::compiled_frame_entry`, the only consumer that wants one, could do
nothing but hard-code `LINE_NUMBER_UNKNOWN`. The tuple is now an
`ActiveCompiledFrame` carrying the bci.

The id is not trusted on sight. `activation_bci` requires the artifact to also
name it as one of **its own** recorded safepoints
(`find_oop_map_for_safepoint_id`). The slot is written before a call, so between
calls it holds the *previous* safepoint; an artifact that reserves no slot
leaves uninitialised memory there. Both are rejected, and an unusable id keeps
the old answer. The original page's rule stands — a wrong line is worse than
none — but a right one is now available for the overwhelming majority of frames.

(3) falls out of the same bci. `drop_osr_continuations` was deleting the one
thing that knew where the OSR-entered method actually was: the continuation
itself. The interpreter `Frame.pc` of an OSR-entered activation stops advancing,
so every later trace named the loop it tiered up in. The dropped activation's
bci is now handed to the frame that survives — an override applied during
capture, deliberately **not** a write to `Frame.pc`, which is live execution
state a diagnostic path has no business moving.

Pinned by `regression-suite/src/RJitStackTraceLines.java`, which asserts no line
*constants*: the same site is reached cold and hot and the two traces must
agree, so editing the file cannot make it vacuously true. It fails on the
pre-fix binary (`osr: probe has no line number (-1)`) and its output is
byte-identical to HotSpot's on the fixed one.

> The vector throws **explicitly**. A first draft dereferenced null and failed
> on **HotSpot**, not on CratonVM: `OmitStackTraceInFastThrow` is on by default
> and replaces a repeated implicit exception's trace with a frameless shared
> one. A cross-VM oracle has to be reachable without a `-XX:` flag the suite
> does not pass.

## (5) — an exception raised BY compiled code loses its frames. PARTIAL

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
              HotSpot                      before                  now
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

## (2) — an inlined callee contributes no frame. OPEN

`active_compiled_frames()` is flat: one entry per compiled artifact. HotSpot's
equivalent is a `ScopeDesc` *chain* — an inlined callee is a nested scope on the
same PC, which is what makes `leaf`/`mid`/`outer` reappear.

The emitter-side shape, from reading it rather than guessing:

* `inlining.rs` **never assigns `cur_bc_pc`**, so a safepoint inside an inlined
  body records the CALLER's invoke bci. That is why `probe:42` is recoverable at
  all, and it means the chain must be recorded *separately* rather than by
  moving `cur_bc_pc` — which is simultaneously the safepoint id, the deopt
  resume point and the GC map key.
* The natural record is a side table keyed by the same `native_pc_offset` the
  oop maps use, filled where `safepoint.rs` already writes
  `bytecode_pc: self.cur_bc_pc`, from an inline stack maintained across
  `try_emit_nested_inline`'s recursion. A side table rather than a field on
  `OopMapEntry`, which is on the GC's hot path and sized deliberately.
* **`try_emit_nested_inline` snapshots and rolls back eleven pieces of emitter
  state.** Any new table must join that list, or a rolled-back splice leaves
  phantom frames in the map — worse than the missing frames, because a phantom
  frame reads as real.

## (4) — UNREPRODUCED, and the lever the page used is a no-op

The original page reported a fourth issue — a compiled activation emitted *in
addition to* its interpreter frame — isolated with:

```
CRATONVM_JIT_NO_INLINE=1  →  hot=... leaf:9 leaf:-1 mid:-1 outer:-1 main:34
```

**`CRATONVM_JIT_NO_INLINE` does not exist.** It is absent from
`types/tests/flag-surface.txt` and nothing in the tree reads it; the only
substring match is the unrelated `CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP`. That
arm ran the default configuration and isolated nothing. The quoted output's
method names and line numbers (`hot=`, `leaf:9`, `main:34`) match no committed
probe either, so it came from one that was never checked in.

Setting the variable on the shipped witness changes nothing, on the pre-fix and
post-fix binaries alike, and `probes/StackTraceCompiledCallee.java` — the real
version of that arm, done in Java by passing the inline size cap — shows no
duplicated frame. Whether (4) exists is an open question rather than a finding;
it needs a reproducer before it needs a fix.

## Reproducers

```sh
# (1), (3), and the shape (2) still breaks
java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceAfterOsr
cratonvm -cp probes StackTraceAfterOsr
CRATONVM_DISABLE_JIT=1 cratonvm -cp probes StackTraceAfterOsr

# (5) — a compiled, non-inlined callee
java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceCompiledCallee
cratonvm -cp probes StackTraceCompiledCallee
CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1 cratonvm -cp probes StackTraceCompiledCallee
CRATONVM_DBG_STTRACE=1 cratonvm -cp probes StackTraceCompiledCallee   # snapshot census
```

Run (5)'s arms **several times**: which frames carry a line depends on what was
compiled at that instant, and one run is not a measurement.

## Also still open, and unrelated to the JIT

The interpreter's NPE messages never build the JEP 358 ` because "…" is null`
clause: HotSpot says `Cannot load from int array because "StProbe.table[...]" is
null`, CratonVM's interpreter says `Cannot load from int array`, and the
compiled path says `null`. A pure `native-builtins` fidelity gap; it belongs in
its own page.

## Why this matters more than it looks

Every logging framework, every `catch (Exception e) { log.error("…", e); }`,
every Spring/Hibernate/Jackson diagnostic and every crash report reads
`getStackTrace()`. On a cold path they are correct. On a hot path — the one a
production incident is about — they named the wrong line, omitted the frames
that would have identified the caller, and, when compiled code was what raised,
omitted the raising method entirely. An operator reading such a trace is not
told that anything is missing.

It also silently weakens two things inside the VM that read the same walk:
`resolve_caller_class_id` (the JEP 403 deep-reflection gate) and the
`Class.forName` caller-loader lookup already had to grow
`frame_class_ids_with_compiled` for the *frame* half of this problem.
`frame_class_ids_with_compiled` shares the fixed walk, so it inherits (1) and
(3); it still inherits (2) and (5).
