# JIT: json-smart parse corruption blamed on the compiled-callee direct entry — CLOSED (2026-07-28)

**Status: ✅ CLOSED.** The symptom is gone; the attribution in the original
write-up was wrong twice over, and both corrections are worth keeping because
the reasoning that produced them is easy to repeat.

The corruption is a json-smart round-trip probe returning one of the parsed
document's own keys, or `ClassCastException: java.lang.Object cannot be cast to
JSONArray`. It was attributed to `4f280090f`, which flipped
`direct_virtual_compiled_callee_entry_enabled()` default-ON. That path is
**exonerated**: `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` left the corruption
unchanged, and the two defects that actually produced it are elsewhere.

## What it actually was

Two independent defects, either of which closes the symptom on its own:

1. **A chain entry's `exact_rbp` could describe another method's frame.** Every
   compiled prologue publishes its own RBP into the innermost-RBP mirror, and
   two generated-code paths reach a compiled callee with no guard at all (the
   inline MIC/PIC cascade and `runtime_lowering::emit_hashed_vtable_stub`, the
   latter not gated by `direct_jit_callee_calls_enabled()` — which is why
   `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` never stopped it). After such a call the
   chain entry named the caller while `exact_rbp` named the callee's frame, so
   `scan_one_frame_precise` published the caller's oop-map offsets against the
   callee's frame and sized the innermost conservative band with the caller's
   `osr_frame_size` — under-covering while returning `true`, which suppressed
   the whole-band fallback. Fixed on dev as `751f65d35`; 0 errors in 6,000,000
   ops against 4 in 4,150,000 without it.

2. **The rbp-chain walk in `remap_active_jit_frames` used an unvalidated parent
   link** — it dereferenced and rewrote slots at `[parent_rbp - …]` before
   checking that `parent_rbp` was aligned, ascending, and inside
   `[scanner_sp, entry_sp)`. Details, the gdb capture, and the measurements are
   in the retired `jit-precise-handler-frame-drops-live-locals-20260727`
   write-up; that is also where the RBC.6 precise-handler-frame relaxation this
   symptom got entangled with is closed out.

## Two corrections to the original analysis

**"It needs a MOVING young generation" is backwards.** `../../../types/src/flags.rs` sets
`DEFAULT_MOVING_YOUNG = false` and parses `CRATONVM_MOVING_YOUNG` by
`present()`, never reading the value — so `CRATONVM_MOVING_YOUNG=0` turns the
compacting young generation **ON**, and the opt-out is
`CRATONVM_NO_MOVING_YOUNG`. The arm that produced the conclusion measured the
moving collector. Re-measured on one binary with the correct spellings:

| config (`-Xmx1g`) | ops | errors |
|---|---|---|
| `CRATONVM_NO_MOVING_YOUNG=1` (i.e. the default) | 4,150,000 | **4** |
| `CRATONVM_MOVING_YOUNG=1` (compacting young) | 4,120,000 | **0** |

The corruption belongs to the **default, non-moving** mark-sweep young
generation. Never infer a flag's polarity from its name in this repo.

**The `4f280090f` attribution came from an A/B at a heap size where the flag
changes the failure rate, not the cause.** Budget 4.5M–6M operations per
configuration at the default heap (~1.3 errors per 1,000,000 ops), or use
`-Xmx64m`, which turns the same failure into "first error by iteration ~20,000".

## Refuted — do not re-litigate without new evidence

**"Almost certainly the same bug as the NodeConnections SIGSEGV."** Run under
`CRATONVM_JIT_POISON_FREE=1`, which retires code with `mprotect(PROT_NONE)` and
never recycles an address, so a call into a retired body becomes an immediate
SIGSEGV instead of a silent wrong answer. **It does not crash**, and the error
rate is unchanged (3 per 1.5M). The corruption is not a call into retired or
recycled code.

**Inline-cache slots holding an unowned entry.** `JitMICSlot::update` and
`JitPICSlot::write_entry`/`install_megamorphic` refuse to publish an entry they
cannot retain (`jit_entry_publishable`), and `CRATONVM_DBG_JIT_STALE_IC=1`
reports zero unowned publications and zero live slots pointing at a retiring
body across full ElasticSearch runs.

## Reproduction

```bash
JS=<…>/json-smart-2.6.0.jar; AS=<…>/accessors-smart-2.6.0.jar; ASM=<…>/asm-9.10.1.jar
TMPDIR=/data/tmp <cratonvm> --java-home <jdk25> -Xmx64m \
  -cp "<classdir>:$JS:$AS:$ASM" JsonSmartProbe3 40000
```

The probe sources live in `docs/known-issues/repros/jsonsmart/`.
