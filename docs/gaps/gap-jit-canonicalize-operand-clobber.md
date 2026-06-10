# Gap: JIT `canonicalize_stack` clobbers live values around conditional branches

**Discovered:** 2026-06-09 (audit fallout from the ternary-in-loop-increment fix)
**Severity:** High — fundamental operand-stack discipline bug in the template JIT; fires in any
method where a register-allocated local is on the operand stack BELOW frame-resident values at a
forward conditional branch. Needs enough interfering locals to exhaust the 7 callee-saved GPRs
(Windows), i.e. big methods — exactly the commons-math/FFT class of code.
**Status:** **Fixed** (branch `fix/jit-ternary-loop-increment`) — alias-safe parallel-move
canonicalization + canonicalize-before-pop in all conditional-branch handlers + swap repair.

---

## Symptom

A comparison `a < b` inside an expression that also has other values on the operand stack
(e.g. `k + (a < b ? 7 : 13)`) takes the wrong arm once the method is JIT-compiled, when the
method has more live int locals than callee-saved registers. With `--nojit` the result is
correct.

## Repro

`test-tern/CanonClobber.java` (in this branch): 14 interfering int locals — 7 hot ones win the
callee-saved registers, 5 cold ones spill. Probe lines build `[CalleeSaved, Frame, Frame]`
at an `if_icmp`:

```
java         CanonClobber 2000   # TOTAL -1263101872  (reference)
cratonvm     CanonClobber 2000   # TOTAL -1629374166  (pre-fix: miscompiled)
cratonvm --nojit CanonClobber 2000  # TOTAL -1263101872
```

## Root causes (four related defects)

1. **`canonicalize_stack()` was not alias-safe.** It relocated stack position `i` to canonical
   offset `base_spill + i*8` in ascending order with one temp. A `CalleeSaved`/`Scratch`/`Xmm`
   slot occupies a stack position but no frame slot, so Frame slots pushed above it sit one
   8-byte slot LOWER than their canonical position; the ascending store at position `i` then
   lands exactly on a higher position's still-unread source and destroys it (e.g.
   `[CalleeSaved(R12), Frame(base+0)]`: storing R12 to base+0 destroys position 1's value
   before it is relocated to base+8). `flush_scratch_registers` and `swap` could additionally
   produce inverted/cyclic shapes. **Fix:** resolve the relocations as a parallel-move problem —
   only emit a move whose target is not a pending source; break cycles by parking one value in
   RCX.

2. **Conditional-branch handlers popped operands BEFORE canonicalizing** (`ifeq..ifle`,
   `if_icmpeq..if_icmple`, `ifnull`/`ifnonnull`). The popped operand's frame slot was invisible
   to `canonicalize_stack()`, which then stored a remaining register-resident slot over it —
   the CMP/TEST read the register local's bits instead of the operand. **Fix:** canonicalize
   before the pops (post-pop-stack-non-empty + forward target), so the operands participate in
   the relocation; in the `if_icmp` handler this also runs before the cmov-min/max consult.

3. **`if_acmpeq`/`if_acmpne` never canonicalized nor recorded `branch_target_stack_depth`** —
   a taken edge with non-empty remaining stack reached a merge with a layout the two paths
   never agreed on (in practice usually surfacing as a simulated-stack underflow that bailed
   the method to the interpreter). **Fix:** same treatment as the other conditional branches.

4. **`swap` of two frame-resident values was a NET NO-OP** (main loop): the handler exchanged
   the two frame slots' memory AND pushed the slot entries in swapped order — the two
   transformations cancelled out. The inline-callee `swap` clobbered value2's slot before
   reading it (first `push_from_rax` reuses the just-reclaimed lower slot), duplicating value1.
   Both swaps also left `next_spill_offset` rewound below the re-pushed live slots, so a later
   push could be handed a live slot. **Fix:** exchange memory keeping entries at their original
   positions (main), read both operands before pushing (inline), `reset_spills()` after.

## FFT hypothesis — tested, disproven

The three residual commons-math `FastFourierTransformerTest` JIT-only failures
(`testSinFunction` 0.007 drift, `testStandardTransformFunction` ~1e-15 drift,
`testAdHocData` "Cannot write field 'normalization' because the object is null") were a
plausible match for this bug (big FFT methods exhaust the register budget). Verified with this
fix applied: all three still fail with IDENTICAL signatures (7/10 pass, 25.3s, JDK 25). They
are a distinct JIT defect — still open, still `--nojit`-clean.

## Audited and NOT affected

- `goto` (0xa7): pops nothing; correct once `canonicalize_stack` itself is alias-safe.
- `tableswitch`/`lookupswitch`: pop the selector but never canonicalize, and record no depth.
  A switch with non-empty remaining stack compiles its case arms against an EMPTY revived
  stack, which makes the simulated stack underflow at the next consumer → safe bail to the
  interpreter (a missed-compile, not a miscompile). Left as-is.
- Inline-callee branch handling (`try_emit_inline_body`): requires the callee operand stack to
  be empty at every branch and merge, bails otherwise.

## Regression tests

`cargo test -p cratonvm-jit --release`:
- `test_if_icmp_canonicalize_preserves_popped_operands` — `[CalleeSaved, Frame, Frame]` if_icmp
- `test_ifxx_canonicalize_preserves_popped_operand` — `[CalleeSaved, Frame]` ifle
- `test_swap_frame_frame_single` — single Frame/Frame swap (the double-swap test was identity-blind)
- `test_swap_then_push_no_live_slot_reuse` — post-swap spill-cursor discipline
- `test_swap_mixed_reg_frame` — register/frame mixed swap

Plus `test-tern/CanonClobber.java` end-to-end vs HotSpot, and the existing
`test-tern/Tern3.java` (gap-jit-ternary-in-loop-increment) must stay OK.
