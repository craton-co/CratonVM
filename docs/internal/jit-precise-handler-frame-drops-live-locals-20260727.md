# JIT: the RBC.6 precise-handler-frame relaxation — CLOSED (2026-07-28)

**Status: ✅ CLOSED.** The relaxation is back ON by default
(`precise_handler_frames_enabled`, opt out with
`CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES`). Four defects were behind the
corruption that forced it off on 2026-07-27; a fifth, unrelated to the gate, was
found on the way. All five are fixed, with a deterministic regression fixture
(`vm/tests/resources/cratonvm/JitPreciseHandlerFrame.java`) that reproduced
every one of them.

Retained because two earlier readings of this symptom were **wrong** and someone
will otherwise re-derive them: it is not "the frame handoff loses live values",
and it is not "the corruption needs a moving young generation".

## What the relaxation is

`try_compile_inner` (`jit/src/lib.rs`) refuses to compile a method whose
exception handler — or code reachable from it — reads a local beyond the
incoming parameters, because `route_jit_exception_through_method` can only
rebuild a handler frame from `this` + declared params. `83e078aa5` relaxed that:
such a method compiles when every throwing site inside its protected ranges is
an `invokestatic`/`invokevirtual`/`invokespecial`/`invokeinterface`/monitor op,
each of which publishes a precise exceptional frame (deopt reason 9). Without
it, ordinary `try`/`catch` methods stay interpreted forever —
`net.minidev.json.JSONValue.toJSONString`,
`org.apache.tomcat.util.buf.CharsetCache.getCharset` (~15us interpreted against
~2us compiled), `StringCache.toString`.

## Symptom that closed the gate

`docs/known-issues/repros/jsonsmart/` (json-smart 2.6.0, real JDK 25, parse →
serialize → re-parse) at `-Xmx64m` fails inside ~20,000 iterations with the gate
open: a re-parse returns one of the document's own keys, a
`ClassCastException: java.lang.Object cannot be cast to JSONArray`, or a
SIGSEGV. `-Xmx64m` is the amplifier — the same failure needs ~1,500,000
operations (25+ min) at the default heap.

```bash
JS=<…>/json-smart-2.6.0.jar; AS=<…>/accessors-smart-2.6.0.jar; ASM=<…>/asm-9.10.1.jar
TMPDIR=/data/tmp <cratonvm> --java-home <jdk25> -Xmx64m \
  -cp "<classdir>:$JS:$AS:$ASM" JsonSmartProbe3 40000
```

Exactly **one** json-smart method is admitted by the relaxation:
`JSONValue.toJSONString(Object, JSONStyle)` — `StringBuilder sb = new …; try {
writeJSONString(value, sb, …) } catch (IOException) {} return sb.toString();`.
Protected range [8,14), the only invoke at pc 11.

## The five defects

### 1. The rbp-chain walk used an unvalidated parent link — the actual crash

`remap_active_jit_frames` (`vm/src/jit/conservative_roots.rs`) walks the JIT
saved-RBP chain after a moving collection and remaps each ancestor frame's oop
slots. It read `parent_rbp` out of the stack and passed it straight to
`remap_one_jit_frame`, which dereferences `[parent_rbp - sp_id_slot_off]` and
**writes** every slot the oop map names. The `parent_rbp <= child_rbp` sanity
check ran only afterwards, and alignment / the `[scanner_sp, entry_sp)` band were
not checked at all. `refresh_moving_young_coverage_for_current_thread` had the
same inversion. The three sibling walks in that same file already validate
first — these two were the outliers.

Caught under gdb:

```
remap_one_jit_frame () at vm/src/jit/conservative_roots.rs:2509
    let sp_id = (unsafe { (id_addr as *const usize).read() }) as u32;
rax  0xffffffffffffffe0        <- 0 - sp_id_slot_off
#4  remap_active_jit_frames ()
#5  update_all_roots () at vm/src/memory/gc.rs:581
#11 jit_invoke_virtual_mic () at vm/src/jit/helpers.rs:8186
```

**This is not specific to the gate.** Compiling `toJSONString` merely made the
chain deep enough to walk. Measured, 40,000-iteration probe runs at `-Xmx64m`:

| build | SIGSEGV |
|---|---|
| dev, gate ON | 3 of 4 |
| dev, gate ON, admitted but compiled with NO precise-frame codegen | 1 of 2 |
| dev, gate ON, reason-9 stubs suppressed | 2 of 2 |
| + this fix only, gate ON | 0 of 3 |
| all fixes, gate ON by default | 0 of 7 |

The middle two rows are what proved the crash is the walk and not the
precise-frame machinery.

### 2. The exceptional frame was keyed on the wrong bci

`emit_post_invoke_exception_check` recorded the reason-9 snapshot at
`deopt_resume_bci` — the invoke's *successor* — which is right for a snapshot
you RESUME at, and wrong for this one: `route_jit_signal_exception` uses the
frame's bci as the **throw pc** for the handler's `[start_pc, end_pc)` test, and
javac routinely ends a protected range exactly at that successor.
`toJSONString`'s range is [8,14) with the invoke at 11 → throw pc 14 → no
handler matched and the exception escaped its own catch block. The snapshot is
now keyed on the throwing instruction, in its own map
(`exc_frame_box_ptr_by_bci`) so it can never share a box with a reason-2/6/7/8
point, and is only emitted for pcs a handler actually protects.

### 3. An exceptional frame is not an ordinary deopt frame

It was stashed in `LAST_DEOPT` and stamped `DeoptReason::ReceiverTypeChanged`.
Every other consumer of that stash treats it as "resume this method at `bci`":
`try_resume_trapped_callee` runs the callee's frame to completion from the
post-invoke bci with the call's result missing from the operand stack and the
exception still pending; the first-call tier-up sink resumes it the same way and
de-speculates the method under the wrong reason; and `jit_dispatch_threw`
consults `has_last_deopt()` to decide whether an `i64::MIN` is a real sentinel,
so a frame nobody claimed makes an unrelated later J/D/F call site bail.

It now has its own `DeoptReason::PendingException`, its own stash
(`deopt::take_exceptional_frame`), and one consumer. Two sinks that handle a
JIT-raised exception without going through that consumer — the first-call
tier-up path and the OSR bail — drop a frame naming **their own** method and
leave a callee's alone. That distinction is load-bearing: dropping the callee's
frame at the OSR bail made `buildStep`'s handler read its `StringBuilder` as
null, because the OSR bail only re-stashes the exception and a later drain
routes it through the callee's own table.

### 4. Two liveness consumers still had no exception edges

`SafepointPublishPlan`'s documented precondition was "safe because
`local_handler_reads_unsafe_local` refuses to compile any method whose handler
reads a non-parameter local" — i.e. safe **only while this gate is closed**. The
same was true, undocumented, of register allocation: without exception edges a
local that ONLY the handler reads is dead throughout the protected range, so it
interferes with nothing there and the colorer may hand it the register of a
local that IS live across the try. The precise frame then reconstructs the
handler's local from that register and reads the other local's object.

`build_cfg_with_handlers` is now shared by the per-pc liveness behind the
snapshot, `plan_safepoint_publication`, and `allocate_registers_with_handlers`.
The backend passes handler ranges only for methods compiled with precise
exceptional frames, so every other compile is byte-identical. Regression test:
`regalloc::tests::handler_only_local_does_not_share_a_register_with_a_live_local`
asserts the interference edge itself, not a coloring outcome.

### 5. `athrow_bci` leaked across compiled-method boundaries — NOT gate-related

`JitSignals::athrow_bci` is the bci of the `athrow` that produced the pending
exception. It carries no method identity, and it survives the callee→caller
hand-off, so a compiled caller's drain range-checks its own exception table
against a pc from the callee:

```
plainStep   protected range [0,4)   catch (Boom) { return i * 2; }
maybeThrow  athrow at bci 13

route_jit_exception_through_method ENTER plainStep throw_pc=13 … handler_pc=None
```

`plainStep`'s handler reads only parameters, so this shape has always compiled
and needs none of the precise-handler machinery — **it is a live defect on
default settings**: 5,619 of 20,000 iterations lost their own `catch` on
unmodified `dev` with the gate OFF (real JDK: 0, `CRATONVM_DISABLE_JIT=1`: 0).
Fixed two ways, because there are two exits from a compiled callee: the dispatch
helper clears the bci when it hands a callee's exception back to the compiled
caller, and the drain accepts the bci only when it indexes an `athrow` in its own
code (which also covers the baked JIT→JIT direct call, where no helper runs).
Falling back to the "throw pc unknown" sentinel is the pre-RBC.6 behaviour —
typed handlers still match by class.

## Regression fixture

`vm/tests/resources/cratonvm/JitPreciseHandlerFrame.java`, three shapes, each
returning a mismatch count that must be 0 (counts, not checksums, so a
duplicated loop iteration from an OSR bail re-checks instead of corrupting an
expected value). Measured with `cratonvm.Drive`:

| build | plain | build | scope |
|---|---|---|---|
| real JDK 25 | 0 | 0 | 0 |
| dev, gate OFF | **5619** | 0 | 0 |
| dev, gate ON | **6171** | **6389** | **3812** |
| all fixes, gate ON (3 runs) | 0 | 0 | 0 |

`plain` is defect 5, `build` is defects 2+3, `scope` is defect 4.

## Two wrong readings, for the record

**"The frame handoff still loses live values."** That was inferred from the
first error moving from ~1,000 to ~18,000 iterations after the exception-edge
liveness fix landed. The remaining failures were the rbp-chain walk (defect 1),
which the gate only exposed. A throwaway build that admitted the method but
compiled it with no precise-frame codegen at all still crashed, which is what
settled it. (Those investigation knobs were not kept; rebuild them from the
table above if the question ever comes back.)

**"The corruption needs a MOVING young generation."** `types/src/flags.rs` sets
`DEFAULT_MOVING_YOUNG = false` and parses `CRATONVM_MOVING_YOUNG` with
`present()`, which never reads the value — so the `CRATONVM_MOVING_YOUNG=0` arm
that produced that conclusion was running the compacting collector, not the
absence of one. The opt-out is `CRATONVM_NO_MOVING_YOUNG`. Re-measured
correctly, the corruption belongs to the default non-moving young generation.

## Superseded write-ups

The `jit-virtual-direct-entry-json-corruption-20260727` write-up (same symptom
seen from the GC side) is retired alongside this one. Its
`CRATONVM_JIT_POISON_FREE` and `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` refutations
still stand and are why `4f280090f`'s direct-entry path was eventually
exonerated.
