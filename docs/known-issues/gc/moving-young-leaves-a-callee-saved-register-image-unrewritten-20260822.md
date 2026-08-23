# A moving young collection leaves a compiled frame's callee-saved register image unrewritten

## Status

**OPEN, no failure attributed.** The gap is measured and a repair exists behind
`CRATONVM_REGISTER_IMAGE_REMAP=1` (default OFF). What is missing is a workload
that fails because of it — the one that looked like it did turned out to be
`bug-generational-ntru-unpinned-jit-reference-20260821.md`, which is a formatter
bug and is now fixed.

## The gap

`band_slot_is_verifiable` (`vm/src/jit/conservative_roots.rs`) splits a compiled
frame's band in two:

* words it INSPECTS are verified — an unpublished movable oop in one of them
  makes `moving_young_unpublished_frame_oop_present` report
  `UNPUBLISHED_FRAME_OOP`, and the cycle diverts to the non-moving sweep;
* words it SKIPS are the prologue's callee-saved GPR/XMM save areas, the
  per-safepoint blind GPR spill, the outgoing-argument / deopt-register reserve,
  and operand-spill slots above the safepoint's live cursor.

`remap_active_jit_frames` rewrites exactly the slots an oop map names, which is
a subset of the first half. So the skipped words are **neither verified nor
rewritten**, and `scan_compiled_frame_bands` READS them — which is what keeps the
referenced object alive across the pause and therefore what gets it COPIED by a
moving cycle.

The justification for skipping them is in the code and is half right: they are
register IMAGES, "not storage this frame resumes from". True of this frame,
false of its caller. A compiled prologue saves the CALLER's callee-saved GPRs
into its own frame and the epilogue pops them straight back into the caller's
registers, so the caller resumes from exactly the words the verifier declined to
look at.

## The observation

`CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` walks the same bands the conservative
scan reads, AFTER the remap, and reports any word that is still a KEY of the
pause's `pointer_map` — an address the collection moved away from. On
`PolynomialTest` under `-XX:+UseGenerationalGC --Xmx 1g`, four runs:

```text
[jit-stale-after-remap] frame-band
  method=java/text/DecimalFormatSymbols.getInstance:()Ljava/text/DecimalFormatSymbols;
  off=80 region=callee-saved-gpr-image verifiable=false
  value=0x20043797498 moved_to=0x20054400a30
```

One hit in four runs, on the one collection of that run that was allowed to move
(`[jitroots] ... incomplete=false ... scan_added=5`). `verifiable=false` is the
verifier saying it never looked. The frame is the interpreter→JIT boundary
frame, so the saved registers are the Rust caller's.

Two things this does NOT establish, and they are the reason this page is open
rather than a bug being fixed:

* **that the word was live.** A register image is full of dead register values;
  a dead word naming a moved-from address costs nothing.
* **that it ever caused a failure.** It correlated with the failing run in the
  four-run sample that first surfaced it. That correlation is what made it look
  like the cause of the ntru failure, and it did not survive: the ntru failure
  is a reclaimed formatter argument, reproduces with `--nojit` (no compiled
  frames at all), and reproduces with the moving young generation switched off.

## The repair, and why it is opt-in

`remap_register_image_words` closes the partition: every word of a live compiled
frame becomes either VERIFIED or REWRITTEN. It rewrites exactly the words
`band_slot_is_verifiable` refuses, and only when the word is a key of
`pointer_map`.

That is the same interpretation the conservative scan already committed to when
it marked the word and kept the object alive — but marking and writing do not
carry the same risk. Over-marking is over-retention; over-writing corrupts. A
caller's callee-saved register holding a non-pointer that happens to equal a
moved object's from-address would be rewritten to a value that is wrong for
whatever it actually was.

So it ships OFF. Measured with it ON against OFF, ABBA-interleaved, one binary,
eight reps per arm on the then-failing ntru fixture: 4 SIG / 8 with it on,
5 SIG / 8 with it off — i.e. no effect on that failure (correctly, since that
failure was the formatter) and no regression from having it on.

## What would close this

A workload where the detector fires on a word the frame then RESUMES from. The
detector already reports the region and the method, so the missing half is a
failure to attribute. Candidates worth pointing it at: any compiled workload
that survives a moving young cycle with `incomplete=false` — those are the only
cycles that can produce the gap, and on this fixture they are roughly one
collection in six.

## Reproducing

```bash
CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1 CRATONVM_DBG_JIT_ROOTSCAN=1 \
  cratonvm --java-home /data/toolchain/jdk-25 -XX:+UseGenerationalGC --Xmx 1g ... \
  2>&1 | grep -E 'jit-stale-after-remap|incomplete=false'
```

`CRATONVM_DBG_JIT_STALE_BELOW_RBP=1` additionally walks the Rust / interpreter
frames beneath the innermost compiled frame. That range is NOT read by the
bounded band scan, so most of what it holds is dead stack slop and it drowns out
the compiled-frame findings — it is a separate flag for that reason, and its
hits are not evidence of anything on their own.

`CRATONVM_REGISTER_IMAGE_REMAP=1` turns the repair on; with it on the detector
reports nothing, which is the check that the two cover the same words.
