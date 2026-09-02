# A warmed-up stack trace is wrong: missing lines, missing frames, and a whole frame set lost when compiled code raises

| | |
|---|---|
| **Status** | **CLOSED 2026-09-02.** Five defects fixed; all seven residuals the 2026-09-01 page left open are closed — five by code, one by a proof that it is unreachable, one because it was stale when it was written. Two further defects were found by measuring the closure and are fixed here. Opened 2026-09-01 on `dev` @ `56d6c3722`, re-scoped the same day, merged binary measured 2026-09-02 @ `eb79f5904`, residuals closed on `fix/stacktrace-residuals-20260902`. |
| **Symptom** | A stack trace captured after warm-up lost frames, printed `-1` for the line of a compiled frame, and reported the OSR back-edge instead of the real call site. Cold traces were correct. Nothing threw and nothing logged. |
| **Cause** | Five independent defects with one habit under them: *a program point that was already recorded somewhere was not carried to the one consumer that wanted it.* The bci was in the frame's safepoint-id slot; the inlined callees were known to the splicer; the OSR continuation was known to the entry site; the trapping bci was in the emitter's hand; the callee's `ClassId` was in the resolver's hand. Each was dropped one function short of the walk. |
| **Fix** | Carry each of them — plus the one thing that was genuinely absent: a translation from the optimizing tier's safepoint COUNTER to a bytecode index. |
| **Verified** | Both witnesses are byte-identical to HotSpot 25 in every row; every kill switch reverts exactly its own half; the frame-line census attributes every remaining `-1`. See **The residual round, measured**. |

**Severity, as filed:** high for diagnosability, zero for program results. Every
trace degraded silently and only *after* warm-up — that is, only in the runs
anyone cares about — and the degraded trace looked plausible.

---

## Scoreboard

### The five defects (2026-09-01)

| # | Defect | State |
|---|---|---|
| 1 | A compiled frame carries no line number | **FIXED** — `b2543ae13` |
| 2 | An inlined callee contributes no frame | **FIXED** — audit branch |
| 3 | An OSR-entered frame reports its back-edge, not its call site | **FIXED** — `b2543ae13` |
| 4 | A compiled activation emitted beside its interpreter frame | **FIXED** — audit branch, on an opcode proof |
| 5 | An exception raised BY compiled code loses every compiled frame | **FIXED** — `70c49bc2d`, completed 2026-09-02 |

### The seven residuals (2026-09-01), and the two found closing them

| # | Residual | State |
|---|---|---|
| 1 | `frame_class_ids_with_compiled` cannot see an inlined method — a blind spot in a security-relevant walk | **CLOSED**: the level carries the resolver's own `ClassId` |
| 2 | (5) is partial — no line on the recovered frame, an unaccounted drain site, run-to-run variance | **CLOSED**: three doors wired, a trap-site side channel added |
| 3 | "The merged binary has not been run" | **STALE WHEN WRITTEN** — the same page's own later section reports the run |
| 4 | The optimizing (IR) tier still keeps `-1` | **CLOSED**: `CompiledMethod::safepoint_bci_table` |
| 5 | x86-64 only; aarch64 frames still report no line | **UNREACHABLE** — proof below |
| 6 | The bci lookup no longer counts its own evidence | **CLOSED**: a refusal census with a column per verdict |
| 7 | The NPE `because "…" is null` clause is never built | **CLOSED**: HotSpot's `a[...]` for an index it cannot name |
| 8 | *(found here)* `after_main_osr` collapses to `len=1 [main:66]` about 1 run in 80 | **CLOSED** — it is residual 2's door, and the same fix |
| 9 | *(found here)* the NPE snapshot duplicates the OSR continuation | **FIXED** — the snapshot now runs through the live path's own dedupe |

---

## The two witnesses

Both are checked in, both compiled with `-g`, and both throw from ONE site
several times in one process with only the amount of prior warm-up differing.

`probes/StackTraceAfterOsr.java` — the callees are small enough to be
**spliced**, so every frame the trace needs is described by one artifact:

| row | HotSpot 25 | CratonVM 2026-09-01 (opened) | CratonVM now |
| --- | --- | --- | --- |
| `before_any_warm` | `len=5 [leaf:25 mid:26 outer:27 probe:42 main:54]` | same | same |
| `after_helper_warm` | `len=5 [leaf:25 mid:26 outer:27 probe:42 main:58]` | same, **27 of 30** | **28 of 28** |
| `after_main_osr` | `len=5 [leaf:25 mid:26 outer:27 probe:42 main:66]` | `len=3 [leaf:25 probe:-1 main:62]` | identical |

`probes/StackTraceCompiledCallee.java` — the callee is padded past
`MAX_INLINE_BYTECODE_SIZE` (325) so it is **compiled and called**, which is the
shape the first probe cannot reach:

| row | HotSpot 25 | CratonVM 2026-09-01 | CratonVM now |
| --- | --- | --- | --- |
| `cold` | `[big:42 probe:56 main:65]` | same | same |
| `after_warm` | `[big:42 probe:56 main:67]` | `[probe:56 main:67]` | identical |
| `after_osr` | `[big:42 probe:56 main:71]` | `[main:71]` | identical |

`CRATONVM_DISABLE_JIT=1` restores every row on the same binary and the same
`.class` file. That is the A/B, and it is why none of this was ever a
class-file, `LineNumberTable` or resolution problem.

---

<<RESIDUAL ROUND MEASURED>>

---

## The switches, and which of them depend on which

There are now nine, and reading them as nine peers gets the wrong answer. They
are two dependency chains and two free-standing names.

```
CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1      reverts the bci recovery on BOTH backends
 |                                          — and takes (2) and (3) down with it, because
 |                                            `probe` is the innermost activation of its
 |                                            chain entry and owns no return address, so
 |                                            both the inline-frame lookup and the OSR
 |                                            override are reached through that bci
 |-- CRATONVM_JIT_NO_IR_FRAME_LINES=1        reverts the OPTIMIZING tier's half alone
 |-- CRATONVM_JIT_NO_OSR_PC_REFRESH=1        reverts (3) alone

CRATONVM_JIT_NO_INLINE_FRAME_MAP=1          reverts the producer AND the walk
 |-- CRATONVM_JIT_NO_INLINE_MISS_EDGE_POISON=1  reverts the emitter-side refusal alone
 |-- CRATONVM_JIT_NO_INLINE_CALLER_FRAMES=1     reverts only the SECURITY walk's expansion
 |-- CRATONVM_JIT_NO_NPE_TRAP_LINES=1           reverts the trap-site table alone

CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1        reverts (5) whole
CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1          reverts BOTH dedupe rules
 |-- CRATONVM_JIT_NO_CALL_FRAME_DEDUPE=1     reverts rule 2 alone
```

The one that is easy to misread is the last child of the second chain.
`CRATONVM_JIT_NO_NPE_TRAP_LINES` reverts the trap table on its own, but
`CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` **also** takes it down — measured, and by
design. The enclosing-method bci of a trap inside a spliced body is the
outermost splice's `entry_bci`, and only the inline-frame session's scope stack
knows it. With that session closed there is no way to tell a callee's pc from
the compiling method's, so the emitter records nothing rather than a bci out of
another method's code. So `big:-1` under `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1`
is the dependency, not a defect.

An arm that changes nothing means that half never engaged, which is a different
finding from that half not working. Do not read the two as one.

---

## Residual 1 — the security-relevant blind spot

`stackwalker::frame_class_ids_with_compiled` is the walk behind
`lang_class::resolve_caller_class_id` (the JEP 403 deep-reflection gate and the
member-modifier check), `class_for_name_one_arg_caller_loader`,
`lang_system::requesting_loader_id`, `classloader::latest_user_defined_loader_class`
and `unsafe_natives_ext::unsafe_caller_is_boot_path`. It reported **one class
per compiled artifact** and so could not see a method the JIT had inlined.

The 2026-09-01 page ranked it top and left it open for a good reason: the walk
answers in `ClassId`, takes no `ClassStore`, and **resolving a JIT label by
NAME to settle a security question is a guess.** A 93-line audit in that
function's own doc comment argued no consumer can ever be standing on an
inlined frame; the argument holds and is kept, but it had to be re-derived
whenever a gate moved.

**It is no longer a guess.** `jit_bridge::resolve_inline_site_from` already
knows the id — it is what it looked the spliced body up by — and dropped it.
`InlineSite::class_id` now carries it, `InlineFrameLevel::class_id` records it
in the artifact, and `InlinedLevel::class_id` hands it to the walk. The id is
chosen by the SAME branch that picks `class_name` (`declaring_id` for a
receiver resolution, `cp_class_id` for a constant-pool one), so the two cannot
name different classes.

**The other half of the refusal is kept rather than argued around.** Only a
chain keyed on this activation's own RETURN ADDRESS is expanded
(`ActiveCompiledFrame::chain_exact`). One `cur_bc_pc` covers a whole spliced
region and is shared with the inline cache's MISS EDGE, where the spliced body
did not run: a wrong frame in a trace is a wrong frame, but a wrong frame in
the JEP 403 gate is a fail-OPEN caller. A level with `class_id == 0` ends the
expansion *including everything below it* — the same break-not-continue rule
the display path follows, because a hole re-parents every deeper level.

`CRATONVM_JIT_NO_INLINE_CALLER_FRAMES=1` restores the flat answer. It is a
separate name from `CRATONVM_JIT_NO_INLINE_FRAME_MAP` on purpose: that one
kills the producer and takes the DISPLAY frames with it, and a
caller-attribution change has to be attributable on its own.

Two things came free with it. The display path takes the recorded id too, which
retires an O(n) `find_class_id_by_name` scan per inlined level per throw and
closes the same-name-under-two-loaders hole the by-name route had; the memo
stays as the fallback for an artifact that recorded no id.

---

## Residual 2 and defect 5 — an exception raised BY compiled code

An implicit NPE in compiled code is not thrown where it happens. The null check
is an inline `TEST`/`JZ` to a stub (`x64/arrays.rs`); the stub calls
`jit_npe_with_action`, loads the `i64::MIN` deopt sentinel and **runs the
epilogue**; the `java/lang/NullPointerException` is constructed afterwards,
from the interpreter. `fillInStackTrace` therefore runs on a stack the compiled
frames have already left.

`70c49bc2d` snapshotted those frames inside the helper and spliced them back.
Three things were still wrong.

### (a) The snapshot reached ONE door out of four

`materialize_implicit_signal` is the constructor for an implicit signal routed
into a compiled callee's **own** handler — which is exactly what
`StackTraceCompiledCallee.probe()`'s `catch (NullPointerException)` is once
`probe` itself is compiled and called from an OSR'd `main`. It attached
nothing, so `after_osr` read `[main:71]` where HotSpot reads three frames.
**That is the "drain site still unaccounted for" the page said was the next
thing to find.**

Two more doors put the FLAG back through the ordinary setter after draining it:
`restash_jit_signals` and `handle_compiled_callee_deopt_sentinel`'s restore.
The ordinary setter takes a SECOND snapshot — one frame shallower, because the
callee that raised has already returned — and silently replaced the real one.
`restash_implicit_signal` did the same to a snapshot that had never been
drained at all.

The attach now lives in the constructor arm rather than at each door, which is
what stops a fifth door from reopening it, and the restash paths put back the
frames (and the JEP-358 action code, which every restash silently dropped)
that were taken with the flag.

> **The shape to remember.** `70c49bc2d`'s own note said *"a signal that is not
> part of `DrainedJitSignals` is a signal some path will drop."* Being in
> `DrainedJitSignals` was necessary and not sufficient: the drain moved it out
> correctly and the RESTASH could not put it back, because the only way to
> raise the flag also re-sampled the payload.

### (b) The recovered frame had no line

`big:-1`. An inline null check is not a GC-capable call, so it publishes no
safepoint id and `activation_bci` correctly refuses the stale one in the slot.
The 2026-09-01 page put the options as "the sp-id slot — which also selects the
oop map for GC — or a per-site `MOV imm32` on every array null check", judged
neither worth a line number, and concluded that closing it *"needs a side
channel that is not the GC's"*.

There is a third option, and it is the cheap one: **put the side channel on the
COLD path.** The emitter records `(trapping bci, splice chain)` per site into
`CompiledMethod::npe_trap_map` and gives a described site ten cold bytes of its
own —

```
MOV <arg0>, imm32(action | site_id << 8)   ; 5
JMP rel32 -> that action's existing stub    ; 5
```

— so the id reaches `jit_npe_with_action` packed into the argument it already
takes. The `TEST`/`JZ` fast path is byte-for-byte what it was; only the `JZ`'s
destination differs, and a site the emitter could not describe still branches
straight to the shared per-action stub.

Keys are per-compile **monotonic ids, never indices**. That is the whole safety
argument: a key that outlived a rewound splice, or one read against the wrong
artifact, MISSES — it cannot land on a neighbouring site and hand a frame a
line from somewhere else. It is also why nothing has to truncate the table on a
rollback.

The guarded-splice body-copy path deliberately drops the key (`0`) on every
copy: a duplicated body is not guaranteed to be the same splice — a guarded
site emits one copy per receiver variant, each a different callee — and
carrying the key would name one variant's chain on all of them. A missing line
is the other outcome, and it is the acceptable one.

`CRATONVM_JIT_NO_NPE_TRAP_LINES=1` reverts the recording and the trampoline
together.

### (c) The snapshot duplicated the OSR continuation — found by measuring

With (a) and (b) fixed, `after_osr` read
`[big:42 probe:56 main:71 main:71]` in 3 of 3 runs, and the *spliced* witness
grew a second `main` in 10 of 28. The snapshot holds every compiled activation
that was on the stack at the trap, including `main`'s OSR continuation — which
is the same activation as the interpreter frame that is still there and that
the late capture emits anyway.

The live capture drops exactly such an entry (`drop_osr_continuations`); the
snapshot path applied neither of its two rules. It now runs through the SAME
function, so the two cannot drift and both dedupe kill switches cover both.

**This is what residual 8 was.** The 2026-09-01 page recorded
`after_main_osr` collapsing to `len=1 [main:66]` about 1 run in 80 and guessed
it was defect 5 surfacing in that probe. It was: the same door, and once the
door was wired the collapse became a duplicate instead — the frames arrived,
unfiltered.

---

## Residual 4 — the optimizing tier

`activation_bci` refused an `used_ir_backend` artifact outright, and the reason
was sound: `ir_lower` stores a monotonic safepoint **counter** starting at 1 in
`OopMapEntry::bytecode_pc`, not a bci. Those counters are small integers
indistinguishable from plausible bcis **and the artifact's own table records
them**, so the confirmation passes and a real, confidently wrong line comes
out.

That was the largest remaining population of `(Unknown Source)` frames, and it
had already produced one silent wrong answer that a test was passing on. From
`RJitStackTraceLines.java`'s own comment:

> It answered correctly here for one reason only: `probe()`'s
> `invokestatic outer` sits at **bci 1** (`javap -c`), and the counter's first
> value is also **1**.

The lowerer now records the real `(id, bci)` pair for every safepoint it emits
(`CompiledMethod::safepoint_bci_table`), passed through `resume_bci` so a
safepoint inside an IR-spliced callee reports the ENCLOSING invoke rather than
a pc that does not exist in this method's `Code`. The walk reads the bci
THROUGH that table, and **the refusal is now exactly as wide as the hazard**:
an id the table does not name is still refused.

`CRATONVM_JIT_NO_IR_FRAME_LINES=1` restores the blanket refusal. It is separate
from `CRATONVM_JIT_NO_COMPILED_FRAME_LINES` because that one reverts the
recovery on both backends at once and so cannot say whether a suspect line came
from the translation or from the slot read.

`RJitStackTraceLines`' assertion is unconditional again, and still carries no
line constant: the hot line must equal the COLD one.

---

## Residual 5 — aarch64: the refusal is UNREACHABLE

Not "conservative", and not "needs hardware". **An aarch64 compiled activation
cannot be on the stack while a Java-level stack capture runs.**

A trace is captured at a throw or from a caller-sensitive native, and both are
reached by an `invoke*`. `Arm64Backend::emit_invoke` sets `self.failed`
**unconditionally** — there is no call-target resolution in that backend at all
— so any method containing a call bails and is interpreted. `athrow` and every
object-model opcode bail on the same rule, so such a body cannot raise either,
and `label_for_pc` refuses every backward branch target, so it cannot loop.
What compiles there is leaf, straight-line, exception-free arithmetic that runs
to its `ret`.

`Arm64CompileResult::oop_maps` is unconditionally empty and its only writer
fails closed, so there is no safepoint of any kind to name a bci at. A line
number is downstream of a safepoint mechanism that backend does not have;
building it is a consequence of building that mechanism, not a separate task.

Three existing tests pin the premises, so a change that makes an aarch64 frame
reachable from a capture trips them first:
`invokestatic_arm_exists_but_always_bails`,
`object_model_opcodes_are_all_unsupported`,
`compiled_methods_carry_no_oop_maps`. The argument is recorded in
`activation_bci`'s own doc comment, beside the `sp_id_slot_off == 0` refusal it
explains.

---

## Residual 6 — the census, and why a hit rate would not have done

The audit branch's `bci_lookup_census` went with the two-source lookup it
described, and the lesson recorded in its place was:

> a silent fallback that produces a plausible answer is indistinguishable, from
> the outside, from the precise path working.

A bare `answered / total` pair has the same defect one level up. A trace prints
`(Unknown Source)` for **five different refusals plus two kill switches**, and
nothing else in the system could tell them apart: an artifact with no
safepoint-id slot, an id naming no map of its own, an untranslated optimizing
id, a value outside the JVMS bci range, and a switch someone left set in an
environment all look the same to a reader.

`cratonvm_jit::compiled_frame_line_counts()` gives each of them a column, every
exit of `activation_bci` names one, and `CRATONVM_DBG=jit-method-stats` prints
the row. Two emitter-side censuses ride along —
`inline_call_map_at_return_counts` (the exact return-address key against the
coarse one) and `inline_miss_edge_poison_counts` (chains deliberately given
up) — both of which were `pub fn`s with **no caller anywhere in the tree**,
which is the same defect this row exists to fix one level up.

A third census counts the two dedupe rules
(`stack_walk_dedupe_counts`). That one exists because rule 2's revert shape is
asserted by no test *and cannot honestly be*: the only arm that ever claimed to
isolate it used `CRATONVM_JIT_NO_INLINE=1`, a variable that does not exist and
never did, so that arm ran the default configuration. A counter cannot say a
rule is right; it can say whether it ever FIRES, and a permanent zero is itself
a finding.

---

## Residual 7 — `because "…" is null`

HotSpot 25: `Cannot load from int array because "StackTraceCompiledCallee.table[...]" is null`.
CratonVM: `Cannot load from int array`.

The backward expression analysis was already there and already wired into the
interpreter's array opcodes. What it did was **bail the whole clause** when it
could not classify the array INDEX: the witness's own shape is
`table[i & 7][0]`, and `iand` is not a modelled producer.

The ellipsis is not a partial answer, it is HotSpot's answer.
`BytecodeUtils` renders a constant, a local and a field load and prints `...`
for everything else, so `a[i & 7]`, `a[i + 1]` and `a[f()]` are all `a[...]`.
The ARRAY operand still bails the whole clause and must: `...[0]` names no
expression at all, where `a[...]` names one and elides only the subscript,
which is exactly the distinction HotSpot draws.

---

## `after_helper_warm`, and a correction to a correction

The 2026-09-01 page recorded this row answering correctly in **27 of 30** runs,
found the same 27/30 on plain `dev` *before* any of the inline-frame work, and
concluded the residual predated that work and wanted its own investigation. It
was right that it predated it. It was also residual 4.

The three failures were line-only (`leaf:-1`, `mid:-1`) — a compiled frame
whose `activation_bci` refused. With the optimizing tier's translation in
place the row is **28 of 28**, and the census says why: `ir=1..3` frames
answered per run, `ir-untranslated=0`.

That is the value of a refusal census stated as plainly as it can be: the
symptom was known for a day, the cause was one of five populations, and no
instrument could say which. The 2026-09-01 revision's own methodological point
still stands and is the reason the row was believed rather than dismissed —
ten runs cannot separate 100% from 90%, because a clean sweep happens about 35%
of the time at p=0.9.

---

## The regression tests

| test | covers |
|---|---|
| `regression-suite/src/RJitStackTraceLines.java` | (1) and (3) **and now (4)'s tier**, against HotSpot 25. Asserts no line constants: the same site is reached cold and hot and the two traces must agree, so editing the file cannot make it vacuously true. |
| `vm/tests/stack_trace_across_tiers.rs` | all four rows of the spliced witness, plus `CRATONVM_JIT_NO_COMPILED_FRAME_LINES`, `CRATONVM_JIT_NO_INLINE_FRAME_MAP` and `CRATONVM_JIT_NO_OSR_PC_REFRESH`. Uses `CRATONVM_DISABLE_JIT=1` on the SAME binary and `.class` file as the oracle, so it can never go stale on a probe edit. |
| `vm/tests/stack_trace_compiled_callee.rs` | **new** — defect (5) end to end: the frame that RAISED is present in all three rows, no frame reports a non-positive line, the interpreter arm agrees byte for byte, and `CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT` / `CRATONVM_JIT_NO_NPE_TRAP_LINES` each still revert their own half. |

> The compiled-callee test is a `vm/tests` test rather than a
> `regression-suite` vector for a measured reason: the oracle would have to be
> HotSpot, and HotSpot's `OmitStackTraceInFastThrow` is ON by default and
> replaces the trace of a repeated implicit exception with a shared frameless
> one. That trap already cost `RJitStackTraceLines` a draft, which dereferenced
> null and failed on **HotSpot**, not on CratonVM. A cross-VM oracle has to be
> reachable without a `-XX:` flag the suite does not pass.

`CRATONVM_JIT_NO_CALL_FRAME_DEDUPE` is still deliberately not asserted — see
residual 6 for what it has instead.

---

## What was ruled out, and how

* **The dedupe was not the cause of (3).** `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1`
  changed nothing on the spliced witness. That single arm eliminated the
  duplicate-frame machinery as the explanation for the stale line before any
  code was written.
* **`CRATONVM_DISABLE_JIT=1` restores everything** — same binary, same `.class`
  file — so none of this was ever a class-file, `LineNumberTable` or resolution
  problem.
* **`CRATONVM_JIT_NO_INLINE=1` separates nothing. It does not exist.** It is
  absent from `types/tests/flag-surface.txt`, nothing in the tree reads it, and
  the only substring match is the unrelated
  `CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP`. The arm that used it ran the
  default configuration and isolated nothing, and the quoted output's method
  names match no committed probe. `probes/StackTraceCompiledCallee.java` is the
  real version of that arm, done in Java by padding the callee past the inline
  size cap.
* **`CompiledMethod::inlined_methods`** names every spliced callee but is a
  flat *set* keyed by nothing; it cannot say which callee a PC is inside.
* **The deopt caller chain** is PC-indexed and retained, and is still wrong for
  this: every level stamps `method_key: self.method_key` — the *compiling*
  method — and the invoke arm inside a splice deliberately publishes no point
  at all. Making it publish one would record a resume bci the enclosing method
  does not have, which is the `IndexOutOfBoundsException`-into-`InternalError`
  regression of 2026-08-28.
* **`OopMapEntry`** is the right key and has no spare field, and inside a
  splice its `bytecode_pc` holds the *enclosing* method's invoke bci — which is
  simultaneously the safepoint id, the deopt resume point and the GC map key.

---

## Two decisions worth keeping

### `Frame::pc` was deliberately NOT advanced

The OSR bci override is applied **during capture** and is **never written back
into `Frame::pc`**. Advancing it would have been one line and would have fixed
the witness. Two independent reasons, either sufficient:

* `Frame::live_locals_mask_here` and `Frame::scan_local_objects_inner` compute
  the per-bci live-locals **root filter** from `[self.pc, self.last_instr_pc]`.
  Moving `pc` to where compiled code really is would make every slot that dies
  between the back-edge and that point **stop being a GC root** — on a frame
  whose locals are the pre-OSR copies the conservative half of the JIT root
  scan is leaning on. A display fix would have become a use-after-free.
* The OSR **safe-reject** exit is correct only *because* `frame.pc` is still
  `entry_pc`. Moving it would resume the interpreter at a bci this activation
  never reached.

Both sessions that touched this reached it independently, which is the
strongest evidence on the page that it is right.

### The fail-closed rule is stricter for a frame than for a line

A wrong line is visibly a line; an inlined frame naming the wrong method is
indistinguishable from a real one. So `push_inlined_chain` stops at the **first
refusal rather than skipping it** — losing a *suffix* of a chain is recoverable
by a reader (the trace is visibly short); a chain with a hole is not. The same
rule now governs `push_compiled_class_ids`, where the consequence of a hole is
not a confusing trace but a wrong answer to a security question.

---

## Why this mattered more than it looked

Every logging framework, every `catch (Exception e) { log.error("…", e); }`,
every Spring/Hibernate/Jackson diagnostic and every crash report reads
`getStackTrace()`. On a cold path they were correct. On a hot path — the one a
production incident is about — they named the wrong line, omitted the frames
that would have identified the caller, and, when compiled code was what raised,
omitted the raising method entirely. An operator reading such a trace was not
told that anything was missing.

---

## Reproducing it

```sh
javac -g -d probes probes/StackTraceAfterOsr.java probes/StackTraceCompiledCallee.java

java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceAfterOsr        # the oracle
cratonvm -cp probes StackTraceAfterOsr                                   # everything ON
CRATONVM_JIT_NO_INLINE_FRAME_MAP=1     cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_OSR_PC_REFRESH=1       cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_COMPILED_FRAME_LINES=1 cratonvm -cp probes StackTraceAfterOsr
CRATONVM_JIT_NO_IR_FRAME_LINES=1       cratonvm -cp probes StackTraceAfterOsr
CRATONVM_DISABLE_JIT=1                 cratonvm -cp probes StackTraceAfterOsr

java -XX:-OmitStackTraceInFastThrow -cp probes StackTraceCompiledCallee   # the oracle
cratonvm -cp probes StackTraceCompiledCallee
CRATONVM_JIT_NO_NPE_FRAME_SNAPSHOT=1 cratonvm -cp probes StackTraceCompiledCallee
CRATONVM_JIT_NO_NPE_TRAP_LINES=1     cratonvm -cp probes StackTraceCompiledCallee

# why each compiled frame did or did not get a line, per run
CRATONVM_DBG=jit-method-stats cratonvm -cp probes StackTraceCompiledCallee
```

Run each arm **several times**. Everything on this page that looked like
flakiness turned out to be a second defect, and one of them was recorded as
1-in-80 for a day.
