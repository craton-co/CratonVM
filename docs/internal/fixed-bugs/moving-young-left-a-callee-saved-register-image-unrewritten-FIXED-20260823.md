# A moving young collection left a compiled frame's callee-saved register image unrewritten

**Status: FIXED 2026-08-23.** The gap the open page measured is closed, the
repair ships ON, and the detector that measured it now prints a census at exit
so `resumed_from=0` is a reading rather than a silence.

Supersedes
`known-issues/gc/moving-young-leaves-a-callee-saved-register-image-unrewritten-20260822.md`,
which was open on one question: whether the stale word was ever LIVE. That
question turned out to be the wrong one to wait on, and §3 says why.

---

## 1. What was open, and what it reproduces as

`band_slot_is_verifiable` (`vm/src/jit/conservative_roots.rs`) splits a compiled
frame's band in two. The words it INSPECTS are verified — an unpublished
movable oop in one of them diverts the cycle to the non-moving sweep. The words
it SKIPS were neither verified nor rewritten, while `scan_compiled_frame_bands`
READS them, which is what keeps the referenced object alive and therefore what
gets it COPIED.

The open page had one hit in four runs. On `dev` at `3ed73bf89`, same fixture,
same flags, it reproduces at a usable rate:

```bash
CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1 CRATONVM_DBG_JIT_ROOTSCAN=1 \
  cratonvm --java-home /data/toolchain/jdk-25 -XX:+UseGenerationalGC --Xmx 1g \
  -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
  -Dtest.java.version.prefix=25 -c "$(cat /data/bcjca-classpath.txt)" \
  junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest
```

```text
[jit-stale-after-remap] frame-band
  method=java/text/DecimalFormatSymbols.getInstance:()Ljava/text/DecimalFormatSymbols;
  off=80 region=callee-saved-gpr-image verifiable=false
  class=java/util/Locale value=0x200437974b8 moved_to=0x20054400a30
```

Three of five unrepaired runs fire, 7 words in total. Every single one of them
is in `callee-saved-gpr-image`; across the whole measurement no other
unverifiable region produced one.

## 2. Why the object moved at all — the part the open page did not name

The open page described WHICH words are unrewritten. It did not say why the
object under them was allowed to move, and that is the mechanism.

`gc_quiescence::is_movable_jit_root(addr)` is a claim about ONE SLOT: "this
reference sits in a precise, rewritable channel (an oop-map entry, a
shadow-stack cell), so you may evacuate it and I will fix the slot up
afterwards." The young sweep's pin set is keyed by OBJECT ADDRESS. So one such
claim, made by one slot, licensed moving the object out from under **every
other word in the process that also held it** — including a frame word nobody
can rewrite.

That is exactly the shape here. The `Locale` is `getInstance`'s argument, so it
is named by a rewritable channel and published movable; the callee-saved image
of the same value is not, and is left holding a from-space address.

## 3. Why "was the word live?" was the wrong question to wait on

The open page listed two things it had not established — that the word was live,
and that it ever caused a failure — and stayed open for them. Neither is
decidable from the outside, and neither needs to be:

* **A register image is popped by the epilogue whether or not the value is
  live.** The prologue saves the CALLER's callee-saved GPRs and the epilogue
  restores them, so "is this word live?" is a question about the caller's
  register allocation, which nothing here can see.
* **The compiled callers do not need the repair.** A compiled frame reloads its
  live oops from the shadow stack after every safepoint, so its registers are
  refreshed whatever the image held. What needs it is the OUTERMOST compiled
  frame, whose caller is the VM's own Rust code at the interpreter→JIT boundary
  — no reload, resumes from exactly those popped registers. Every observation on
  this defect, on both the open page and here, names that frame.

So the answerable question is not "did this word matter" but "is the write
sound", and that one has an answer.

## 4. The repair, and why it is now narrow enough to ship on

`remap_register_image_words` used to rewrite EVERY word the verifier refuses.
That is what made it look unsafe: most of those words are dead, and a dead word
is precisely where a non-pointer that merely LOOKS like an object base is
plausible. The four regions are not equivalent:

| region | read by anything? | can hold an oop? | rewritten now |
| --- | --- | --- | --- |
| `callee-saved-gpr-image` | **yes** — the epilogue pops it into the caller | yes | **yes** |
| `callee-saved-xmm-image` | yes | no — an XMM never holds a reference here | no |
| `safepoint-gpr-spill-image` | **no** — write-only | yes | no |
| outgoing-args / deopt reserve, spills above the live cursor | no — dead by definition | — | no |

The `safepoint-gpr-spill-image` row is not an inference:
`emit_pre_safepoint_spill` says it in its own words — *"Under the default
non-moving young sweep no post-call reload is needed"* — the spill exists so the
conservative scan can SEE the register file, and nothing ever loads from those
slots.

Narrowing to the GPR image is what makes the write defensible. For a hit there
to be a false positive, a live callee-saved register would have to hold a
non-reference whose value is exactly a young object's base address —
`heap.is_object_address` having validated it against the arena bounds and the
object-start bitmap. Loop counters, sizes and PCs do not reach those addresses,
and Rust-side pointers are in different mappings.

**Measured, ABBA-interleaved, one binary, `CRATONVM_REGISTER_IMAGE_REMAP` as the
only difference:**

```text
arm   runs   stale words in resumed-from regions
on      6    0  0  0  0  0  0
off     5    3  0  1  3  0
```

`OK (14 tests)` on all eleven runs.

## 5. The other half: a PIN, where pinning is possible

A Cheney young collection has no pin — every survivor is copied — which is why
the moving arm needs the write. The NON-MOVING young sweep does have one, and
there the sound repair is to not move the object in the first place.

`gc_quiescence::UNREWRITABLE_JIT_ROOTS` is that veto.
`publish_unrewritable_band_roots` publishes every object an unverifiable band
word resolves to, and `sweep_young_non_moving`'s pin decision reads it as "pin
regardless of any movable claim":

```rust
let movable = honour_movable
    && is_movable_jit_root(a)
    && !is_unrewritable_jit_root(a);
```

The cost is the conservative cost the band scan already pays on the MARKING side
— an `i64` that happens to equal an object address defers that object's
promotion by one cycle — and it can never dangle. Measured on the same fixture:
9 addresses per collection, 1 on the cycle that moves.

`[jitroots]` reports it, so `scan_added>0 unrewritable=0` (every live reference
in verified storage) and `unrewritable=N` (the gap, being closed) are
distinguishable at runtime:

```text
[jitroots] ... scan_added=5 unrewritable=1 ...
[jitroots] ... scan_added=36 unrewritable=9 ...
```

## 6. The detector says zero out loud now

`CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` used to print only when it found
something, which cannot tell a repaired run from a run with no moving cycle at
all, or from a binary that predates the instrument. It now splits its hits and
prints the totals at exit — zeros included — on BOTH exit arms
(`native-builtins`' `System.exit` shutdown trailer, which is the one a JUnit
runner takes, and `vm-cli`'s normal-return arm):

```text
[jit-stale-after-remap] census: resumed_from=0 dead_region=0
```

`resumed_from` counts the callee-saved GPR image. `dead_region` counts the rest,
which is left stale on purpose. A run reporting `resumed_from=0 dead_region=N`
is the repaired state, not a quiet one.

## 7. Kill switch and blast radius

`CRATONVM_REGISTER_IMAGE_REMAP=0` restores the pre-fix behaviour, so the A/B is
inside one binary. The pin veto has no separate switch: it is a strictly
over-approximating pin on a set that averages nine addresses, and turning it off
would only restore a hazard the write then has to repair.

## 8. What this does NOT claim

* **No failing workload is attributed to it.** The ntru failure that first
  correlated with the detector was a formatter bug
  (`bug-generational-ntru-unpinned-jit-reference-20260821-FIXED`) and is not
  this. What is claimed is that the partition is now closed: every word of a
  live compiled frame is verified, rewritten, or provably read by nobody.
* **`dead_region` hits are not zero by construction.** On this fixture they
  measured zero; another workload can produce them, and they are still not a
  defect for the reason §4 gives.
