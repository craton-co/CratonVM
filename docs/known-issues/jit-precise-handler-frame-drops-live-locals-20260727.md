# JIT: the RBC.6 precise-handler-frame relaxation drops live locals (json-smart corruption) — FIXED by re-gating

**Status:** ✅ corruption fixed on dev by closing the gate (default OFF, opt in
with `CRATONVM_JIT_PRECISE_HANDLER_FRAMES=1`). The *feature* behind the gate is
still unsound and stays OPEN for its owner.

This file replaces an earlier writeup of the same symptom that blamed the
compiled-callee direct entry (`4f280090f`). **That attribution was wrong** — it
came from an A/B at the default heap size where the flag changes the failure
rate, not the cause. The bisect below is the real answer. If you read the
earlier version, discard its conclusion, keep its repro.

## Symptom

`docs/known-issues/repros/jsonsmart/JsonSmartProbeWarmed.java` (json-smart
2.6.0, real JDK 25, parse → serialize → re-parse round trip) fails rarely at a
default heap and quickly at a small one:

```
ROUNDTRIP MISMATCH at iter=47981
  doc={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt1={"a":1,"b":2.5,"c":"hello","d":true,"e":null,"f":[1,2,3]}
  rt2="d"                    <-- the re-parse returned one of the KEYS

EXCEPTION ... java.lang.ClassCastException: class java.lang.Object
                              cannot be cast to class net.minidev.json.JSONArray
```

Both shapes say the same thing: **an unrelated object is standing where a live
one used to be.** Not a parser bug, not a character-cursor miscompile — a lost
object. The failing parse is transient (the same parser re-parses the same
document correctly right after).

## Reproduction (2-3 minutes)

```bash
JS=<...>/json-smart-2.6.0.jar; AS=<...>/accessors-smart-2.6.0.jar; ASM=<...>/asm-9.10.1.jar
TMPDIR=/data/tmp <cratonvm> --java-home <jdk25> -Xmx64m \
  -cp "<classdir>:$JS:$AS:$ASM" JsonSmartProbeWarmed 20000
```

**`-Xmx64m` is the whole trick.** At the default heap the same bug needs
~1,500,000 operations (25+ minutes) to show once; at 64 MiB the first failure
lands at iteration 400-5,000, i.e. inside two minutes, because the failure needs
a GC at the wrong moment. `CRATONVM_DISABLE_JIT=1` is clean at 64m, so it is a
JIT×GC interaction, not a heap-sizing bug.

## What it was

`git bisect` over the 27 commits between dev `a7a5d6ff5` (clean, 200k ops) and
`a80673ad0` (fails), building and running the 64m repro at each step, named
**`83e078aa5` "jit: complete precise thin-lock monitor lowering"**.

Nothing to do with monitors. That commit also **relaxed the RBC.6 admission
gate**: a method whose exception handler reads a local beyond the incoming
parameters used to be refused outright (`return None`), because
`route_jit_exception_through_method` can only restore `this` + declared params.
It is now compiled whenever `precise_exception_frame_sites_supported()` finds
only `invokestatic`/monitor ops in the protected ranges, on the promise that
each of those publishes a precise exceptional frame.

The promise does not hold. Closing that one gate — everything else in the
commit kept — makes the repro clean:

| build (200k ops per run, `-Xmx64m`) | first error |
|---|---|
| dev before `83e078aa5` | none |
| dev at/after `83e078aa5` | iteration 400-5,000 |
| + exception-edge liveness fix (below) | iteration ~18,000 |
| + gate closed (this change) | none (3 runs) |
| gate closed, default heap, 1,500,000 ops | none |

`CRATONVM_JIT_DENY` bisection pinned the victim method to
`JSONParserBase$MSB.toString()` — `new String(this.b, 0, this.p + 1)`, reached
from the parser's handler-bearing methods. Denying just that method's
compilation is also clean (3/3 runs), which is what first pointed at frame
reconstruction rather than at the parser.

## Two defects, one fixed properly and one gated

**1. The liveness behind the snapshot had no exception edges — FIXED.**
`regalloc::live_locals_per_pc` builds its CFG from fall-through and branch
targets only. A local that ONLY the catch handler reads is therefore computed
dead at every pc inside the protected range — which is exactly where these
snapshots are taken. The snapshot builder (`x64.rs`,
`build_and_record_deopt_point`) then encodes it as `FrameValue::Undefined`, so
the reconstructed frame loses it; for a reference local that also removes the
GC's view of the object through that frame.

`regalloc::live_locals_per_pc_with_handlers` now models exception edges
(handler pcs become block leaders; every block overlapping a protected range
gains the handler block as a successor), and the x64 back-end receives the
method's exception table through a one-shot request alongside
`set_precise_exception_frame_request`. Regression test:
`live_locals_per_pc_sees_a_local_only_the_handler_reads`.

Same change also fixes a second, independent trap in that consumer: the
snapshot used `live_locals_per_pc` WITHOUT its coverage bitmap, and an
uncovered pc reads as `0` = "nothing live" — i.e. "drop every local". It now
falls back to "everything live" for an uncovered bci, which is what the
coverage bitmap was introduced for.

**2. The relaxation itself is still unsound — GATED OFF.** With fix 1 in place
the repro survives (first error moves from ~1,000 to ~18,000 iterations), so
something beyond the liveness inputs still loses values on this path. Until
that is found, `precise_handler_frames_enabled()` (in `jit/src/lib.rs`) gates
the admission and defaults to **off**, restoring the pre-`83e078aa5` refusal.
`CRATONVM_JIT_PRECISE_HANDLER_FRAMES=1` re-opens it for whoever picks the
feature up.

## For whoever re-opens it

- Use the 64m repro above; it is deterministic enough to bisect a fix in
  2-minute steps.
- The remaining defect is *not* the liveness input, and *not* the callee's own
  compilation (`MSB.toString` is a 19-byte method with no handler of its own —
  it is the frames AROUND it that are reconstructed).
- Suspects not yet excluded: the operand-stack half of the snapshot, the
  `Undefined` encoding for dead REFERENCE locals (the pre-`83e078aa5` code
  deliberately restricted that substitution to non-oop slots, with a comment
  about exactly this GC hazard; `83e078aa5` widened it to every slot), and
  whether reason-9 publication actually covers every throwing site the
  admission check accepts.
- Unrelated finding from the same investigation, already fixed: the MIC
  `MIC_HIT_NOENTRY` publish path stored `cached_entry_ptr` with raw atomics and
  took no owner for the callee artifact — see
  `vm/src/jit/helpers.rs` and
  `docs/known-issues/jit-segv/nodeconnections-retired-jit-code-jump-20260727.md`,
  whose SIGSEGV is that shape.
