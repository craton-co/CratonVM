# ✅ RETIRED — a JIT-compiled method's self-recursive activations were invisible to Java stack walks

**RETIRED 2026-08-19.** The defect this page was opened for is fixed, along with
two more found underneath it. What remained is a different mechanism and has its
own page:
`docs/known-issues/jit/jit-compiled-frame-between-chain-entries-is-invisible-20260819.md`.

Three fixes came out of this page, each with a kill switch and a regression
guard:

| # | defect | fix | guard |
|---|---|---|---|
| 1 | 64 self-recursive activations reported as ONE frame | walk the saved-RBP chain in `active_compiled_frames` | `self_recursive_activations_survive_tier_up` |
| 2 | every fast-tier innermost frame wore its CALLER's name | `cm.compile_id = compiler.compile_id` in `x64/driver.rs` | `SWFrames` at HotSpot parity |
| 3 | an OSR'd method reported twice (interpreter frame + compiled entry) | `drop_osr_continuations`, keyed on `can_osr_enter(frame.pc)` | `an_osr_continuation_is_not_reported_twice` |

Measured, round 39 (post-tier-up), against HotSpot 25:

| probe | before any fix | now | HotSpot 25 |
|---|---|---|---|
| `SWShape` tail / non-tail self-recursion | 3 / 3 | **67 / 67** | 67 / 67 |
| `SWShape` distinct-method chain | 10 | 10 | 10 |
| `SWMutual` mutual recursion | 67 | 67 | 67 |
| `SWCross` total / innermost name | 67 / `recurse` | **68 / `helper`** | 68 / `helper` |
| `SWFrames` (Log4j caller shape) | `false`, n=67 | **`true`, n=68** | `true`, n=68 |
| `SWValue` computed answers | `VALUES_OK` | `VALUES_OK` | `VALUES_OK` |

Everything below is the page as it stood, including the hypotheses that were
refuted along the way — kept because two of them look right from the code and
cost a session each.

---

## The failure

`vm/tests/wp1_9_stackwalker.rs::stackwalker_log4j_deep_repeated_walks_finish_under_jit`
fails on `dev`, and not for the reason its name suggests — nothing times out:

```
AssertionError: expected cratonvm.StackWalkerLog4jStressProbe$LoggerFactory
                but got cratonvm.StackWalkerLog4jStressProbe
```

That probe reproduces Log4j2's caller-class lookup: walk the stack, drop the
`Locator` frames, take the first frame in the owning package. The answer is
wrong because `LoggerFactory.resolveCaller`'s frame is simply **not in the
walk** once the enclosing recursion is compiled — so the first surviving
package frame is `recurse`, declared in the outer class.

## Measured

Azure Linux, `dev` `a392c9ded`, one release binary, real JDK 25 backend.
Probes are committed under `probes/` (`SWFrames`, `SWThrow`, `SWShape`,
`SWMutual`, `SWValue`); each does 32-60 rounds so the first rounds are
interpreted and the later ones compiled.

### The frame count, before and after tier-up

`StackWalker.walk` and `Throwable.getStackTrace()` over a 64-deep direct
self-recursion, same process, early round vs late round:

| arm | early round | late round |
|---|---:|---:|
| CratonVM, JIT on | 67-68 | **3** |
| CratonVM, `--nojit` | 67-68 | 67-68 |
| HotSpot 25 | 67-68 | 67-68 |

65 of 68 frames disappear, and they disappear at the moment the method is
compiled — not gradually.

### Which call shapes lose frames

One process, one binary, three shapes measured side by side (`SWShape`,
`SWMutual`):

| shape | depth | early | late (JIT) | `--nojit` | HotSpot |
|---|---:|---:|---:|---:|---:|
| direct self-recursion, TAIL call | 64 | 67 | **3** | 67 | 67 |
| direct self-recursion, NON-tail | 64 | 67 | **3** | 67 | 67 |
| mutual recursion `a→b→a→b` | 64 | 67 | 67 | 67 | 67 |
| distinct-method chain `c0→…→c7` | 8 | 10 | 10 | 10 | 10 |

**Only DIRECT self-recursion collapses.** Mutual recursion — same depth, same
heat, no two consecutive identical frames — keeps every frame, which rules out
"consecutive duplicate frames are de-duplicated" and rules out "one entry per
distinct method on the stack" (that would report 2 for the mutual case, and it
reports 67).

### The answers are still right

`SWValue` runs non-tail self-recursion whose caller does arithmetic on the
returned value (`sum(100)`, `fact(12)`, `depthCount(64)`), 60 rounds, well past
tier-up: `wrongRounds=0/60` under JIT, `--nojit` and HotSpot alike. So the
compiled activations genuinely exist and unwind correctly; only the *record* of
them is missing.

## Mechanism

`runtime/stackwalker.rs::capture_full_trace` builds the Java-visible stack from
the interpreter's `thread.frames`, then splices in
`jit::conservative_roots::active_compiled_frames()`. Its own doc comment states
the invariant this defect violates:

> A JIT-compiled method runs without pushing a `Frame`, so on its own `frames`
> is the Java stack *minus everything the JIT has taken over*.

`active_compiled_frames` reads `JIT_ENTRY_CHAIN`, which holds **one entry per
interpreter→JIT entry**. A compiled method that calls *itself* from inside
compiled code never re-enters the JIT from the interpreter, so it pushes no
chain entry — 64 activations, one entry.

## The fix

`active_compiled_frames` now walks the saved-RBP chain outward from
`exact_rbp`, exactly as `remap_active_jit_frames`' Stage 5 already does for the
collector (`push rbp; mov rbp,rsp` frames, `[rbp]` = caller RBP, `[rbp+8]` =
return address into the caller, same bound checks and the same 4096 guard),
and emits one entry per activation instead of one per chain entry. The walk is
read-only where Stage 5 rewrites oop slots, and it never reports FEWER frames
than before: a walk cut short by a bound still owes the boundary method.

`CRATONVM_JIT_NO_NESTED_TRACE_FRAMES=1` restores the old answer, so the whole
table below is an A/B **inside one binary**.

| probe (round 39, post-tier-up) | walk ON | walk OFF | HotSpot 25 |
|---|---|---|---|
| `SWShape` tail self-recursion | **67** | 3 | 67 |
| `SWShape` non-tail self-recursion | **67** | 3 | 67 |
| `SWShape` distinct chain | 10 | 10 | 10 |
| `SWMutual` mutual recursion | 67 | 67 | 67 |
| `SWCross` (inlined callee below the recursion) | 68 | 67 | 68 |
| `SWValue` computed answers | `VALUES_OK` | — | `VALUES_OK` |

## What remains OPEN

`SWCross` puts a distinct `helper()` at the bottom of the hot recursion. Its
frame count now matches HotSpot (68), but the frame is named `recurse`, not
`helper`.

> ### ⚠ The "it is inlined" attribution below was REFUTED 2026-08-19
>
> This section used to say the missing frame was inlined into its caller, and
> the design survey after it costed three ways to add inline frame records. Both
> are wrong, and acting on them wasted a session — read the refutation first.
>
> **The missing methods are not inlined. They are fully compiled, with real
> frames.** Measured with `CRATONVM_DBG_JITC` on the two probes:
>
> ```
> inline-resolve REFUSED cratonvm/SWCross.helper()V depth=0: new/anewarray/multianewarray
> ```
>
> `helper()` constructs an exception, so the inline resolver refuses it outright.
> And the Log4j shape has **zero splices in the entire run** (`grep -c
> inline-splice` = 0) while its supposedly-inlined method is compiled standalone
> and called directly:
>
> ```
> full-compile   cratonvm/SWFrames$LoggerFactory.resolveCaller()Ljava/util/List; entry=0x… len=475
> bg-direct-call BOUND cratonvm/SWFrames$LoggerFactory.resolveCaller()Ljava/util/List;
> ```
>
> So the real shape is a **compiled→compiled DIRECT call that pushes no chain
> entry**, and the walk then misidentifies the innermost frame. The `[acf2]`
> diagnostic on `SWCross` shows exactly that: `nested=1`,
> `ret_caller=<non-jit>`, `decoded=<no-decode>`, and the frame-record mirror
> answering `published=cratonvm/SWCross.recurse:(I)V` for a frame that is not
> `recurse`. The innermost frame is misnamed by the mirror, not absent because
> of inlining.
>
> **Two facts that also invalidate the survey's "cheapest" option.** The hot
> methods are admitted to the OPTIMIZING (IR) pipeline, and (a) `IrBuilder::build`
> does not inline at all — `Lowerer::inline_scopes` is empty on every compile,
> as its own doc comment states — and (b) IR's sp-id is a **monotonic counter**
> (`next_sp_id`, starts at 1), not a bytecode pc. The survey's option 2 keyed
> extents by bci and read that slot; it cannot work for an IR-compiled frame.
> The `cur_bc_pc` invariant it rested on holds only for the single-pass backend.
>
> What is actually needed is to identify the innermost compiled frame correctly
> when it was entered by a direct compiled→compiled call. `direct_call_callee`
> is the intended mechanism and returns `<no-decode>` here because the innermost
> frame's `[rbp+8]` resolves to `<non-jit>` — start there, and re-measure before
> assuming anything in this file.

### The misnaming: FIXED 2026-08-19, one line

The innermost compiled frame of every **fast-tier** method was reported under
its CALLER's name. Cause: `CompiledMethod::compile_id` defaults to 0, the
optimizing backend assigns it (`ir_lower.rs`, `cm.compile_id = compile_id`), and
the single-pass backend's finalize **never did**. So the prologue published
`compiler.compile_id` into the identity mirror on entry while `bind_compile_id`
bound `CompiledMethod::compile_id` — which was 0, and `bind_compile_id`
early-returns on 0. The id was therefore never bound, `lookup_compile_id`
answered `None`, and `innermost_frame_method` fell through to the boundary
method.

The `[acf3]` dump is what named it: `cm_id=4 published=<none>` on the innermost
chain entry — an id the frame publishes and the registry cannot resolve. Fix is
`cm.compile_id = compiler.compile_id;` in `x64/driver.rs`, mirroring the line
the IR backend already had.

Measured (`--java-home` JDK 25, `CRATONVM_JIT_THRESHOLD=1` where noted):

| probe | before | after | HotSpot 25 |
|---|---|---|---|
| `SWFrames` (Log4j caller shape) | `hasLoggerFactory=false`, n=67 | **`true`, n=68** | `true`, n=68 |
| `SWCross` innermost frame name | `recurse` (wrong) | **`helper`** | `helper` |

`cargo test -p cratonvm-vm --release --lib`: 2569 passed, 0 failed.

### What is STILL open, and it is a third mechanism

`stackwalker_log4j_deep_repeated_walks_finish_under_jit` remains red. Not
inlining (refuted above), not the misnaming (fixed above): a compiled frame in
the middle of the chain is **unreachable by the walk**.

`SWStress3` instruments the failing walk itself — same call path, same heat, so
it observes the hot stack rather than a cold copy, which is where `SWStress2`
went wrong. Its output:

```
got=cratonvm.SWStress3   saw=Locator.getCallerClass | recurse | recurse | recurse | …
```

`LoggerFactory.resolveCaller` sits between `getCallerClass` and `recurse` on the
real stack and is absent from the walk. It is not inlined (`grep -c
inline-splice` = 0 for this probe too). The shape that produces the gap:
`getCallerClass` has its own chain entry whose `[rbp+8]` is a non-JIT return
address, so the per-entry RBP walk stops immediately (`nested=1`); and
`resolveCaller`, entered by a DIRECT compiled→compiled call from `recurse`, has
no chain entry of its own. It therefore falls between two entries and no walk
covers it.

Closing that needs the walk to cross chain-entry boundaries — one walk over the
whole native stack rather than a band per entry — which is a bigger change than
either fix above and should be measured on its own.

Also visible now that the innermost frame is named correctly: `SWCross` reports
69 frames where HotSpot reports 68, because an OSR'd `main` appears twice —
once as its interpreter frame and once as its compiled chain entry. Pre-existing
and independent; the trace reads `[0] main [1] main [2] grab …`.

That is what still fails
`stackwalker_log4j_deep_repeated_walks_finish_under_jit`: Log4j2's caller lookup
wants `LoggerFactory.resolveCaller`, which is inlined away.

### Design survey, 2026-08-19 — SUPERSEDED, see the refutation above

**Kept only so the dead end is not re-walked.** It reasons about adding inline
frame records, and the premise that the missing frames are inlined is false
(measured; see above). Its one durable finding is the mechanical inventory —
`InlineSite` carries the callee identity, `CompiledMethod` has a single
constructor, `try_emit_inline_site` owns the rollback set — which is still
accurate should real inline frames ever be needed for a DIFFERENT reason.

**The metadata half is nearly free.** `InlineSite` already carries the inlined
callee's `class_name` / `method_name` / `descriptor`, and the emitter knows the
native offsets it is writing (`self.buf.pos()`), so recording an extent table —
`(native_start, native_end, label, owner_class_id)` per spliced body, nested
sites included — is a contained change:

* `CompiledMethod` has exactly ONE constructor (`CompiledMethod::new`), so
  adding a field costs one edit, not twenty.
* `try_emit_inline_site` already has the checkpoint/rollback discipline a new
  vector must join (`exception_check_stubs`, `deopt_stubs`, `forward_patches`,
  … all truncated on bail). Its comment explains what a stale speculative entry
  does to the buffer; an extents vector that skipped that set would be the same
  class of bug.
* `runtime::stackwalker::compiled_frame_entry` wants only
  `(depth, "class/Name.method:descriptor", owner_class_id)`, so a synthesized
  inline frame needs no new consumer-side type.

**The blocker is that a stack walk cannot learn the innermost frame's own PC.**
Expanding an extent table needs a code offset per physical frame. The RBP walk
gives one for every ANCESTOR frame — `[rbp+8]` is the return address into the
caller, i.e. the caller's current PC — but the innermost frame's own PC is the
return address pushed by the call it is currently inside, which lives below
`exact_rbp` in the Rust helper's frame and is not reachable from the chain.

And the innermost frame is exactly the one that needs expanding. Measured, not
assumed: the `[acf]` diagnostic on `SWFrames` reports
`nested=65 [recurse | recurse | …]` — `nested[0]`, the innermost, is the frame
carrying the inlined callee. Same shape in `SWCross`, where the trace is
captured inside the inlined `helper()`.

The frame-record mirror that generated code already maintains is a PAIR — RBP
(`inline_rbp_tls_disp`) and compile id (`inline_cm_tls_disp`), republished after
every call by `emit_post_call_frame_record`. **There is no PC in it.** So the
options are:

1. **Publish the call-site PC as a third mirror slot**, beside the two that are
   already written. Conceptually simple and it makes the extent table
   immediately usable — but it adds a store to every call-out from compiled
   code, which is a throughput cost on the hottest path in the VM and wants its
   own measurement before anyone commits to it.
2. **Key the extents by BYTECODE pc instead of native offset** and read the
   frame's live safepoint id from `[rbp - sp_id_slot_off]`, which costs nothing
   new because the frame already stores it. The catch: inlined bodies map their
   bcis back through `orig_bci`, so a point inside a spliced body reports the
   ENCLOSING invoke's bci — which identifies the inline SITE (sites are keyed by
   caller pc) but is only as fresh as the last safepoint.
3. **Reuse the deopt scope chain.** `FrameState::caller` is populated for
   inlined scopes since 2026-08-18 (`push_inline_scope` / `pop_inline_scope`),
   and it is the richest description available — but it is keyed by
   `native_offset` at DEOPT POINTS, which do not coincide with the arbitrary PC
   a stack walk lands on.

Option 2 is the cheapest and needs no codegen change; option 1 is the most
accurate. Neither is a small enough call to make without measuring, which is
why this page is still open rather than half-fixed.

**A wrong turn worth not repeating.** Resolving the innermost frame
"decode-first" — preferring `direct_call_callee`'s `E8 rel32` decode over the
published-compile-id mirror — looks like the fix for the naming and is not: it
pushed `SWCross` to **69** frames (one spurious entry) and still did not name
`helper`, because there is no `helper` frame to name. Measured and reverted.

## What is NOT established

- **Which emitter is responsible for the pre-fix collapse.** The obvious suspect,
  `ir_lower.rs::emit_self_recursive_call` (a direct `CALL rel32` to the
  method's own entry, bypassing `jit_invoke_dispatch`), is **REFUTED**:
  `CRATONVM_JIT_IR_SELFREC_DIRECT=0` changes nothing, measured in one binary,
  both arms. Some other self-call path is the one that skips the chain push.
- **Whether it is a regression or newly exposed.** The test passed on a
  dev-baseline log from 01:43 the same day, so *something* changed today that
  made these methods compile where they previously did not. The chain-entry gap
  itself may be much older; no bisect was run.
- **Blast radius beyond stack walks.** Anything keyed on frame identity is
  suspect — `Reflection.getCallerClass`, security/caller-sensitive checks,
  Spring's `ControlFlowPointcut`, and `StackOverflowError` depth accounting were
  not measured. Note `invoke.rs`'s tail-call-elimination comment already records
  one such casualty (`ControlFlowPointcutTests`) for the *interpreter* TCE and
  restricts TCE to self-recursive calls for exactly this reason; the JIT is now
  reintroducing the same invisibility on the compiled path, for non-tail calls
  too.

## Reproduction

```bash
javac -d /tmp/swprobes probes/SWShape.java probes/SWMutual.java probes/SWValue.java
./target/release/cratonvm --java-home <jdk25> -cp /tmp/swprobes cratonvm.SWShape
./target/release/cratonvm --java-home <jdk25> --nojit -cp /tmp/swprobes cratonvm.SWShape
<jdk25>/bin/java -cp /tmp/swprobes cratonvm.SWShape
```

Expected on a fixed VM: `tail`, `nonTail` and `chain` hold their early-round
counts in every arm, matching HotSpot.

The failing suite test is

```bash
cargo test -p cratonvm-vm --release --test wp1_9_stackwalker \
  stackwalker_log4j_deep_repeated_walks_finish_under_jit
```

## Related

- `vm/src/runtime/stackwalker.rs` — `capture_full_trace` /
  `interleave_compiled_frames`, and the `interp_depth` splice contract.
- `vm/src/jit/conservative_roots.rs` — `JIT_ENTRY_CHAIN`,
  `active_compiled_frames`.
- `vm/src/runtime/interpreter/invoke.rs` — the interpreter's own tail-call
  elimination, already restricted to self-recursive calls on stack-trace
  fidelity grounds.
